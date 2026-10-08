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

//! `COMPRESSED` (`80`) **decode** tests — the top-level zlib wrapper.
//!
//! A compressed `term_to_binary` image is
//! `[VERSION_MAGIC=131][COMPRESSED_EXT=80][inner_size:4 BE][zlib stream]`, where the
//! 4-byte size is the inner term's byte length **excluding** the version byte and the
//! stream is a zlib-wrapped (RFC 1950) deflate stream. Ground truth is the legacy C
//! `binary2term_prepare` + `binary2term_uncomp_size`
//! (`../../erts/emulator/beam/external.c:1700-1816`):
//!
//! * the first byte must be `VERSION_MAGIC`;
//! * the wrapper is only taken when the tag is `80` **and** at least 5 bytes follow it
//!   (the tag plus the 4-byte size) — i.e. a 6-byte image minimum, otherwise the `80`
//!   is just an ordinary — and invalid — term tag;
//! * a declared size above `32*1024*1024` (or a failed allocation) makes the C measure
//!   the *real* uncompressed length and require the declared size to equal it;
//! * the inflated length must equal the declared size, else `badarg`;
//! * **bytes after the zlib stream are ignored** (verified against the oracle);
//! * **bytes after the inner term inside the inflated body are ignored too** — the C
//!   hands the inflated buffer to the ordinary term decoder and, like
//!   `binary_to_term/1,2`, never requires it to be fully consumed.
//!
//! The last two points are the ones a hand-rolled decoder gets wrong, so they are
//! pinned here explicitly. Offline (no system `erl` needed); the live
//! oracle-vs-runtime agreement for these shapes is the sibling
//! `../tools/differential` harness (`differential compare 'binary_to_term(<<…>>)'`).

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::prelude::{atom_val, is_atom, is_list, is_nil, AtomTable};

use beam_codec::compress::{compress, decompress, MAX_UNCOMPRESSED};
use beam_codec::term::{cons_head, cons_tail};
use beam_codec::{
    decode_term, encode_term_body, COMPRESSED_EXT, LIST_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT,
    STRING_EXT, VERSION_MAGIC,
};

/// `term_to_binary(lists:duplicate(500, hello), [compressed])` — the oracle image
/// captured from OTP 29.1.1 (regenerate with
/// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(lists:duplicate(500,hello),[compressed])])' -s init stop`).
///
/// Layout: `131, 80, 0,0,13,178 (inner size 3506), <44-byte zlib stream>`; the declared
/// size is the length of `term_to_binary(lists:duplicate(500, hello))` (3507) minus its
/// version byte.
const COMPRESSED_DUP_500_HELLO: [u8; 50] = [
    131, 80, 0, 0, 13, 178, 120, 156, 237, 197, 177, 9, 0, 32, 16, 4, 176, 183, 112, 84, 65, 228,
    192, 210, 197, 29, 192, 53, 44, 146, 38, 169, 106, 247, 244, 57, 146, 45, 73, 146, 36, 73, 250,
    168, 245, 0, 145, 146, 3, 87,
];

/// `term_to_binary([1,2,3])` — the plain image of the small term used by the
/// hand-built wrapper cases (the oracle emits `STRING_EXT` for a byte list).
const PLAIN_123: [u8; 7] = [VERSION_MAGIC, STRING_EXT, 0, 3, 1, 2, 3];

/// A heap generous enough for the 500-element seed list and the atom table of a
/// single decode.
fn new_heap() -> Heap {
    Heap::new(4096).expect("the carrier allocator never fails for this size")
}

/// Wraps `body` in the `[131][80][size:4 BE][zlib]` envelope, declaring `size` as the
/// inner size (which the caller can deliberately get wrong).
fn compressed_image(body: &[u8], declared_size: u32) -> Vec<u8> {
    let zlib = compress(body, 6).expect("zlib of a tiny body");
    let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    image.extend_from_slice(&declared_size.to_be_bytes());
    image.extend_from_slice(&zlib);
    image
}

/// Decodes `bytes` and asserts the error is exactly `BeamError::BadArg` (a malformed
/// image is never a panic, an `unsupported`, or a wrong term).
fn assert_bad_arg(bytes: &[u8]) {
    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    match decode_term(bytes, &mut heap, &mut atoms) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for {bytes:?}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The oracle seed
// ---------------------------------------------------------------------------

#[test]
fn oracle_compressed_seed_decodes_to_the_original_list() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap();
    let term = decode_term(&COMPRESSED_DUP_500_HELLO, &mut heap, &mut atoms)
        .expect("the oracle's compressed image must decode");

    // The inner term is `[hello, hello, …]` — 500 atoms, proper tail.
    let mut count = 0usize;
    let mut cursor = term;
    while is_list(cursor) {
        let head = cons_head(cursor).expect("a list head");
        assert!(is_atom(head), "expected an atom element, got {head:?}");
        assert_eq!(atoms.name(atom_val(head)), Some("hello"));
        cursor = cons_tail(cursor).expect("a list tail");
        count += 1;
    }
    assert!(is_nil(cursor), "the inner list must be proper");
    assert_eq!(count, 500);

    // The declared size really is the inner body's length: the inflated stream is the
    // plain image minus its version byte.
    let body = decompress(&COMPRESSED_DUP_500_HELLO[6..]).expect("a zlib stream");
    assert_eq!(body.len(), 3506);
    assert_eq!(
        body,
        encode_term_body(term, &atoms).expect("the decoded term re-encodes"),
        "the inflated body must equal the plain encoding of the decoded term"
    );
}

#[test]
fn the_wrapper_is_transparent_for_a_hand_built_image() {
    // `{ok,17}`'s body is `[SMALL_TUPLE_EXT, 2, <atom ok>, SMALL_INTEGER_EXT, 17]`.
    let mut atoms = AtomTable::new();
    atoms.seed(["ok"]);
    let ok = beam_term::prelude::make_atom(atoms.lookup("ok").expect("seeded"));
    let mut body = vec![SMALL_TUPLE_EXT, 2];
    body.extend_from_slice(&encode_term_body(ok, &atoms).expect("encode the atom"));
    body.extend_from_slice(&[SMALL_INTEGER_EXT, 17]);

    let image = compressed_image(&body, body.len() as u32);
    let mut heap = new_heap();
    let mut decode_atoms = AtomTable::new();
    let term =
        decode_term(&image, &mut heap, &mut decode_atoms).expect("a hand-built wrapper decodes");

    // The plain image of the same term decodes to the identical structure: the wrapper
    // is a transport form, not a term of its own.
    let mut plain = vec![VERSION_MAGIC];
    plain.extend_from_slice(&body);
    let mut heap2 = new_heap();
    let mut atoms2 = AtomTable::new();
    let plain_term = decode_term(&plain, &mut heap2, &mut atoms2).expect("the plain image decodes");
    assert_eq!(encode_term_body(term, &decode_atoms).unwrap(), body);
    assert_eq!(
        encode_term_body(plain_term, &atoms2).unwrap(),
        encode_term_body(term, &decode_atoms).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Trailing bytes — the two places the C tolerates them
// ---------------------------------------------------------------------------

#[test]
fn bytes_after_the_zlib_stream_are_ignored() {
    // The C inflates and stops at `Z_STREAM_END`; the bytes after the stream are never
    // looked at (verified against the oracle: appending bytes to a compressed image
    // still decodes). `decompress`/`decode_term` must behave identically.
    let mut image = COMPRESSED_DUP_500_HELLO.to_vec();
    image.extend_from_slice(&[1, 2, 3, 4, 5, 0xFF]);

    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    let term = decode_term(&image, &mut heap, &mut atoms)
        .expect("trailing bytes after the zlib stream must be ignored");

    // Still the 500-element list, not a partially decoded term.
    let mut count = 0usize;
    let mut cursor = term;
    while is_list(cursor) {
        let head = cons_head(cursor).expect("a list head");
        assert_eq!(atoms.name(atom_val(head)), Some("hello"));
        cursor = cons_tail(cursor).expect("a list tail");
        count += 1;
    }
    assert!(is_nil(cursor));
    assert_eq!(count, 500);
}

#[test]
fn bytes_after_the_inner_term_are_ignored() {
    // The C substitutes the *inflated buffer* for the input buffer and then runs the
    // ordinary term decoder; like `binary_to_term/1,2` that decoder never requires the
    // buffer to be fully consumed. So a body of `[<term>][stray bytes]` decodes to the
    // term. (This is the shape that made our runtime raise `badarg` where the oracle
    // returned the term — the sibling differential harness reproduced it with
    // `differential compare 'binary_to_term(<<…>>)'`.)
    let mut body = PLAIN_123[1..].to_vec(); // the term, without the version byte
    body.extend_from_slice(&[0, 0, 0xFF]);
    let image = compressed_image(&body, body.len() as u32);

    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    let term = decode_term(&image, &mut heap, &mut atoms)
        .expect("trailing bytes inside the inflated body must be ignored");

    // The same term as the plain image decodes to.
    assert_eq!(
        encode_term_body(term, &atoms).expect("re-encode"),
        &PLAIN_123[1..],
        "the decoded term must be the whole of `[1,2,3]`, with the stray bytes dropped"
    );
}

#[test]
fn the_plain_path_stays_strict() {
    // Documented asymmetry, and the reason it is invisible: `decode_term`'s *plain*
    // path keeps its historical `Decoder::finish()` strictness (`` `[131,97,17,0]` ``),
    // while the runtime implements `binary_to_term` through `Decoder::decode` — which
    // has no `finish` — so the strictness is never observable through `binary_to_term/1`
    // (the oracle returns `17` there). The compressed branch cannot do the same because
    // the wrapper is handled *here*, in `decode_term`.
    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    assert!(matches!(
        decode_term(&[VERSION_MAGIC, 97, 17, 0], &mut heap, &mut atoms),
        Err(BeamError::BadArg)
    ));
    assert_eq!(
        decode_term(&[VERSION_MAGIC, 97, 17], &mut heap, &mut atoms).unwrap(),
        beam_term::prelude::make_small(17)
    );
}

// ---------------------------------------------------------------------------
// Negative cases
// ---------------------------------------------------------------------------

#[test]
fn below_the_six_byte_minimum_the_tag_is_a_bare_term_tag() {
    // Fewer than 6 bytes: the C needs the tag *plus* the 4-byte size before it takes
    // the `COMPRESSED` branch (`external.c:1759`), so the `80` is decoded as an
    // ordinary term tag — which is not a term and therefore `badarg`.
    assert_bad_arg(&[]);
    assert_bad_arg(&[VERSION_MAGIC]);
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT]);
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0]);
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0]);
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 13]);
    // The same bytes without the version byte: `new` rejects the missing magic first.
    assert_bad_arg(&[COMPRESSED_EXT, 0, 0, 13, 178, 120, 156]);
}

#[test]
fn a_nested_or_non_first_compressed_tag_is_malformed() {
    // `COMPRESSED` is accepted only immediately after the version byte. Nested inside a
    // term — or reached after another tag — it is an unknown tag for the term decoder
    // (`goto error`), never a term.
    assert_bad_arg(&[VERSION_MAGIC, SMALL_TUPLE_EXT, 2, COMPRESSED_EXT, 0, 0, 13, 178, 0]);
    assert_bad_arg(&[
        VERSION_MAGIC,
        SMALL_TUPLE_EXT,
        2,
        97,
        1,
        COMPRESSED_EXT,
        0,
        0,
        13,
        178,
        0,
    ]);
    // A list whose head is the wrapper tag.
    let mut nested = vec![VERSION_MAGIC, LIST_EXT, 0, 1, COMPRESSED_EXT];
    nested.extend_from_slice(&COMPRESSED_DUP_500_HELLO[2..]);
    assert_bad_arg(&nested);
}

#[test]
fn a_wrong_declared_size_is_bad_arg() {
    let body = &PLAIN_123[1..];
    // Too small (1 instead of 6) — the inflated length disagrees.
    assert_bad_arg(&compressed_image(body, 1));
    // Too large.
    assert_bad_arg(&compressed_image(body, 7));
    // The oracle image with its declared size nudged by one.
    let mut wrong = COMPRESSED_DUP_500_HELLO;
    wrong[5] = wrong[5].wrapping_add(1);
    assert_bad_arg(&wrong);
    // Zero, with a *valid* zlib stream of an empty body: the wrapper is accepted
    // (`0 == 0`) and the empty body is then not a term.
    assert_bad_arg(&compressed_image(&[], 0));
}

#[test]
fn a_declared_size_over_the_cap_is_bad_arg() {
    // The C measures the real uncompressed length when `dest_len > 32*1024*1024` and
    // requires the declared size to equal it (`external.c:1768-1777`), so an inflated
    // body shorter than an over-cap declaration is `badarg`. `MAX_UNCOMPRESSED` pins the
    // same 32 MiB bound.
    let body = &PLAIN_123[1..];
    assert_eq!(MAX_UNCOMPRESSED, 32 * 1024 * 1024);
    assert_bad_arg(&compressed_image(body, MAX_UNCOMPRESSED as u32));
    assert_bad_arg(&compressed_image(body, u32::MAX));
}

#[test]
fn truncated_or_corrupt_zlib_streams_are_bad_arg() {
    let image = COMPRESSED_DUP_500_HELLO;

    // Truncation at every offset past the 6-byte header: the deflate stream or its
    // Adler-32 trailer is incomplete, so zlib fails — never a panic and never a
    // partially decoded term.
    for len in 6..image.len() {
        assert_bad_arg(&image[..len]);
    }

    // A corrupted Adler-32 trailer.
    let mut corrupt = image.to_vec();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert_bad_arg(&corrupt);

    // A corrupted deflate block (flip a byte in the middle of the stream).
    let mut corrupt = image.to_vec();
    corrupt[20] ^= 0xFF;
    assert_bad_arg(&corrupt);
}

#[test]
fn a_non_zlib_payload_is_bad_arg() {
    // A plausible length with a payload that is not a zlib stream at all.
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 10, 1, 2, 3, 4, 5, 6]);
    // A valid zlib header followed by nothing.
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 1, 0x78, 0x9C]);
    // A valid zlib stream whose *inflated* payload is not a term.
    let garbage = compressed_image(&[0xC8, 0xC8, 0xC8], 3);
    assert_bad_arg(&garbage);
    // A valid zlib stream of an empty body declaring size 0 — see the size test.
    assert_bad_arg(&compressed_image(&[], 0));
}

#[test]
fn decoding_never_panics_on_arbitrary_tail_bytes() {
    // Every byte after a valid wrapper header must produce a typed error or a term,
    // never a panic or an out-of-bounds read.
    for byte in 0u16..=255 {
        let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 1, byte as u8];
        image.extend_from_slice(&[0, 0, 0, 0]);
        let mut heap = new_heap();
        let mut atoms = AtomTable::new();
        let _ = decode_term(&image, &mut heap, &mut atoms);
    }
}