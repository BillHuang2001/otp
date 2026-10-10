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

//! Hostile / malformed external-term input: the decode **robustness net**.
//!
//! `binary_to_term/1` is fed attacker-controlled bytes, so the decoder must never
//! panic, over-read, hang or allocate wildly on a malformed image — every failure is a
//! typed [`beam_common::BeamError`] (the C routes *every* decode failure to `B2TBadArg`,
//! `../../erts/emulator/beam/external.c:1989-1991`). This file builds a broad corpus
//! from the whole tag set and asserts that property:
//!
//! * **(a) truncation** — every valid image, cut at *every* offset `0..len`, is rejected;
//! * **(b) single-byte corruption** — every byte set to `0x00`/`0x80`/`0xFF` must return
//!   a `Result` (no panic / over-read / hang). A single flip can legitimately produce a
//!   *different* valid term (e.g. the value byte of a small int), so only the mutations
//!   that are provably invalidating are asserted to be `Err`: the version byte and the
//!   tag byte (none of `0x00`/`0x80`/`0xFF` is a tag);
//! * **(c) length/count corruption** — liar `STRING_EXT`/`LIST_EXT`/`BINARY_EXT` lengths,
//!   a bit-31 `MAP_EXT` pair count, oversized `LARGE_BIG_EXT` byte counts, an oversized
//!   `LARGE_TUPLE_EXT` arity, `NEWER_REFERENCE_EXT` id counts past `MAX_REF_IDS`, a
//!   non-tiny legacy creation, a first id past `MAX_REFERENCE`, an over-long atom, a
//!   non-finite float — each is `BadArg`;
//! * **(d) `COMPRESSED` attacks** — a short wrapper header, a lying declared size, a
//!   truncated/corrupt zlib stream, a non-zlib payload and a non-term inner body.
//!
//! The exact error *class* is asserted only where the crate pins it elsewhere
//! (`tests/decode_bound_oracle.rs`, `tests/compressed_decode.rs`, `tests/roundtrip.rs`);
//! the generic mutation families assert just "is a typed `Err`". The corpus stays
//! shallow on purpose — the deep-nesting abort is a separate fix under test elsewhere.
//!
//! A representative subset (the valid corpus + the targeted corruption cases +
//! truncations of a few small images) is also cross-checked **live** against the system
//! `erl`'s `binary_to_term/1` classification (`ok` vs `badarg`); the oracle is
//! `$ERTS_ORACLE` if executable, else `erl` on `$PATH`, and its absence is a loud skip,
//! never a silent pass (the convention of `tests/interop.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::compress::compress;
use beam_codec::term::{
    make_big, make_bitstring, make_external_pid, make_external_port, make_external_ref, make_float,
    make_fun_external, make_fun_imported, make_map, FunPid,
};
use beam_codec::{
    decode_term, encode_term, encode_term_body, encode_term_local, Encoder, ATOM_UTF8_EXT,
    BINARY_EXT, BIT_BINARY_EXT, COMPRESSED_EXT, DIST_HEADER_EXT, FLOAT_EXT, FUN_EXT, LARGE_BIG_EXT,
    LARGE_TUPLE_EXT, LIST_EXT, MAP_EXT, NEWER_REFERENCE_EXT, NEW_CACHE_EXT, NEW_FLOAT_EXT,
    NEW_REFERENCE_EXT, PID_EXT, PORT_EXT, RECORD_EXT, REFERENCE_EXT, SMALL_BIG_EXT,
    SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT, VERSION_MAGIC,
};
use beam_term::prelude::{
    make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small,
    AtomTable, Eterm, NIL,
};

/// A decode heap smaller than the production one: a fixed-size bump allocator, so a
/// hostile length/count that slips past the bounds checks exhausts it and returns a
/// typed `Alloc`/`BadArg` instead of a multi-gigabyte request.
const HEAP_WORDS: usize = 1 << 12;

fn new_heap() -> Heap {
    Heap::new(HEAP_WORDS).expect("the carrier allocator never fails for this size")
}

/// `[VERSION_MAGIC] ++ tail` — the `term_to_binary` envelope.
fn wi(tail: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(tail.len() + 1);
    v.push(VERSION_MAGIC);
    v.extend_from_slice(tail);
    v
}

/// Decodes `image` with a fresh heap + atom table (the production `binary_to_term/1`
/// shape).
fn decode(image: &[u8]) -> beam_common::Result<Eterm> {
    let mut h = new_heap();
    let mut atoms = AtomTable::new();
    decode_term(image, &mut h, &mut atoms)
}

/// Asserts the image is rejected with *some* typed error — never a panic, a successful
/// decode or a hang.
fn assert_rejected(image: &[u8], what: &str) {
    match decode(image) {
        Err(_) => {}
        Ok(t) => panic!("{what}: expected a typed Err for {image:?}, decoded {t:?}"),
    }
}

/// Asserts the image is rejected as exactly [`beam_common::BeamError::BadArg`].
fn assert_bad_arg(image: &[u8], what: &str) {
    match decode(image) {
        Err(beam_common::BeamError::BadArg) => {}
        other => panic!("{what}: expected BadArg for {image:?}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (0) The valid-image corpus: one image per tag family the codec can produce
// ---------------------------------------------------------------------------

/// A valid `term_to_binary` image.
struct Valid {
    label: &'static str,
    image: Vec<u8>,
    /// Whether the *system* oracle decodes the exact image in isolation. `false` only
    /// for the `LOCAL_EXT` wrapper and the identifier immediates (see `oracle_cases`).
    oracle: bool,
}

fn enc(label: &'static str, t: Eterm, atoms: &AtomTable, oracle: bool) -> Valid {
    Valid {
        label,
        image: encode_term(t, atoms).expect("a hand-built valid term always encodes"),
        oracle,
    }
}

fn valid_corpus() -> Vec<Valid> {
    let mut atoms = AtomTable::new();
    let a_hello = atoms.intern("hello");
    let a_ok = atoms.intern("ok");
    let a_f = atoms.intern("f");
    let a_m = atoms.intern("m");
    let node = atoms.intern("nonode@nohost");
    let node_term = make_atom(node);
    let mut h = new_heap();
    let mut v = Vec::new();

    // Scalars.
    v.push(enc("small_int", make_small(5), &atoms, true));
    v.push(enc("integer", make_small(1000), &atoms, true));
    v.push(enc(
        "small_big",
        make_big(&mut h, false, &[1u64 << 63]).expect("big"),
        &atoms,
        true,
    ));
    let many = vec![u64::MAX; 33];
    v.push(enc(
        "large_big",
        make_big(&mut h, true, &many).expect("big"),
        &atoms,
        true,
    ));
    v.push(enc(
        "float",
        make_float(&mut h, 3.14).expect("float"),
        &atoms,
        true,
    ));
    v.push(enc("atom", make_atom(a_hello), &atoms, true));
    v.push(enc(
        "binary",
        make_bitstring(&mut h, b"hello", 40).expect("binary"),
        &atoms,
        true,
    ));
    v.push(enc(
        "bitstring",
        make_bitstring(&mut h, &[0b1010_1010], 3).expect("bitstring"),
        &atoms,
        true,
    ));
    v.push(enc("nil", NIL, &atoms, true));
    let tail23 = h.cons(make_small(2), make_small(3)).expect("cons");
    v.push(enc(
        "byte_string",
        h.cons(make_small(1), tail23).expect("cons"),
        &atoms,
        true,
    ));
    // Containers.
    let tail_a = h.cons(make_atom(a_hello), NIL).expect("cons");
    v.push(enc(
        "list",
        h.cons(make_small(1), tail_a).expect("cons"),
        &atoms,
        true,
    ));
    v.push(enc(
        "improper_list",
        h.cons(make_small(1), make_small(2)).expect("cons"),
        &atoms,
        true,
    ));
    v.push(enc(
        "tuple",
        h.tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("tuple"),
        &atoms,
        true,
    ));
    v.push(enc(
        "map_flat",
        make_map(
            &mut h,
            &[
                (make_small(1), make_atom(a_ok)),
                (make_small(2), make_small(3)),
            ],
            &atoms,
        )
        .expect("flat map"),
        &atoms,
        true,
    ));
    let mut pairs = Vec::new();
    for i in 0..40i64 {
        pairs.push((make_small(i), make_small(i * 2)));
    }
    v.push(enc(
        "map_hamt",
        make_map(&mut h, &pairs, &atoms).expect("hamt map"),
        &atoms,
        true,
    ));
    // Funs.
    v.push(enc(
        "export_fun",
        make_fun_external(&mut h, 2, a_m, a_f).expect("export fun"),
        &atoms,
        true,
    ));
    let creator = FunPid {
        node,
        number: 3,
        serial: 0,
        creation: 0,
    };
    v.push(enc(
        "imported_fun",
        make_fun_imported(
            &mut h,
            1,
            a_m,
            &[7u8; 16],
            1,
            0,
            0,
            creator,
            &[make_small(1)],
        )
        .expect("imported fun"),
        &atoms,
        true,
    ));
    // Identifiers: the oracle's own `binary_to_term` may demote a *local-node* id or
    // reject a foreign reference, so the full images are not oracle-compared; the
    // malformed identifier forms are (see `targeted_corpus`).
    v.push(enc(
        "external_pid",
        make_external_pid(&mut h, node_term, 7, 0, 0).expect("ext pid"),
        &atoms,
        false,
    ));
    v.push(enc(
        "external_port",
        make_external_port(&mut h, node_term, 5, 0).expect("ext port"),
        &atoms,
        false,
    ));
    v.push(enc(
        "external_ref",
        make_external_ref(&mut h, node_term, &[1, 2, 3], 0).expect("ext ref"),
        &atoms,
        false,
    ));
    let enc_node = |t: Eterm| {
        let mut e = Encoder::new(&atoms).with_local_node(node, 0);
        e.encode(t)
            .expect("an internal id encodes with a local node");
        e.finish()
    };
    let internal_pid = make_internal_pid(make_pid_data(0, 10));
    let internal_port = make_internal_port(make_port_data(11));
    v.push(Valid {
        label: "internal_pid",
        image: enc_node(internal_pid),
        oracle: false,
    });
    v.push(Valid {
        label: "internal_port",
        image: enc_node(internal_port),
        oracle: false,
    });
    // The `LOCAL_EXT` wrapper carries a node hash the oracle does not share, so the
    // *full* image is ours-only (its truncations still agree — both reject).
    v.push(Valid {
        label: "local_ext",
        image: encode_term_local(make_small(5), &atoms, 0).expect("local_ext"),
        oracle: false,
    });
    // A real `COMPRESSED` wrapper (hand-built, so the corpus always gets the wrapper
    // regardless of the encoder's shrink rule).
    let zeros = make_bitstring(&mut h, &[0u8; 256], 256 * 8).expect("zero binary");
    let body = encode_term_body(zeros, &atoms).expect("body");
    let zlib = compress(&body, 6).expect("zlib");
    let mut compressed = vec![VERSION_MAGIC, COMPRESSED_EXT];
    compressed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    compressed.extend_from_slice(&zlib);
    v.push(Valid {
        label: "compressed",
        image: compressed,
        oracle: true,
    });

    v
}

/// The valid corpus really does decode (otherwise the truncation/corruption net would
/// be vacuous).
#[test]
fn valid_corpus_decodes_cleanly() {
    let corpus = valid_corpus();
    assert!(
        corpus.len() >= 20,
        "the valid corpus shrank to {} images",
        corpus.len()
    );
    for c in &corpus {
        match decode(&c.image) {
            Ok(_) => {}
            Err(e) => panic!("the valid `{}` image must decode, got {e:?}", c.label),
        }
    }
    // The compressed image is really a wrapper (not the encoder's plain fallback).
    let compressed = corpus
        .iter()
        .find(|c| c.label == "compressed")
        .expect("compressed case");
    assert_eq!(compressed.image[1], COMPRESSED_EXT);
}

// ---------------------------------------------------------------------------
// (a) Truncation at every offset
// ---------------------------------------------------------------------------

#[test]
fn truncation_at_every_offset_is_rejected() {
    let mut truncated = 0usize;
    for c in valid_corpus() {
        for len in 0..c.image.len() {
            assert_rejected(
                &c.image[..len],
                &format!("{} truncated to {len} bytes", c.label),
            );
            truncated += 1;
        }
    }
    assert!(
        truncated > 200,
        "only {truncated} truncations were exercised"
    );
    eprintln!("[beam_codec hostile] {truncated} truncations rejected, none panicked");
}

// ---------------------------------------------------------------------------
// (b) Single-byte corruption
// ---------------------------------------------------------------------------

#[test]
fn single_byte_corruptions_never_panic() {
    let mut rejected = 0usize;
    let mut accepted = 0usize;
    let mut flips = 0usize;
    for c in valid_corpus() {
        for i in 0..c.image.len() {
            for byte in [0x00u8, 0x80, 0xFF] {
                let mut m = c.image.clone();
                m[i] = byte;
                // The call must *return* (no panic, no hang, no over-read).
                let res = decode(&m);
                flips += 1;
                if res.is_err() {
                    rejected += 1;
                } else {
                    accepted += 1;
                }
                // The version byte and the tag byte are provably invalidating: neither
                // `0x00`, `0x80` nor `0xFF` is `VERSION_MAGIC` or a wire tag.
                if i <= 1 {
                    assert!(
                        res.is_err(),
                        "{}: flipping byte {i} to {byte:#04x} must be rejected, got {res:?}",
                        c.label
                    );
                }
            }
        }
    }
    assert!(flips > 500, "only {flips} flips were exercised");
    assert!(
        rejected > 0,
        "no single-byte corruption was rejected at all"
    );
    eprintln!(
        "[beam_codec hostile] {flips} single-byte corruptions returned a typed Result \
         ({rejected} rejected, {accepted} happened to still be valid terms), none panicked"
    );
}

// ---------------------------------------------------------------------------
// (c) Length / count / identifier corruption
// ---------------------------------------------------------------------------

/// Malformed images that are `badarg` in *both* this codec and the system oracle (each
/// classification was verified against OTP 29.1.1 / erts-17.1).
fn targeted_corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();

    // Unknown / obsolete / transport tags.
    out.push(("unknown_tag_00", wi(&[0x00])));
    out.push(("obsolete_fun", wi(&[FUN_EXT, 0, 0, 0, 1])));
    out.push(("obsolete_new_cache", wi(&[NEW_CACHE_EXT, 0, 0])));
    out.push(("dist_header", wi(&[DIST_HEADER_EXT, 0])));
    out.push((
        "compressed_nested",
        wi(&[SMALL_TUPLE_EXT, 1, COMPRESSED_EXT, 0, 0, 0, 0]),
    ));
    out.push(("atom_cache_ref_without_cache", wi(&[82, 0])));
    out.push(("local_ext_short_hash", wi(&[121, 0, 0, 0])));

    // Lying lengths.
    out.push(("string_len_10_has_2", wi(&[STRING_EXT, 0, 10, 1, 2])));
    out.push(("string_len_3_has_2", wi(&[STRING_EXT, 0, 3, 1, 2])));
    out.push((
        "list_len_5_has_1",
        wi(&[LIST_EXT, 0, 5, SMALL_INTEGER_EXT, 1]),
    ));
    out.push((
        "binary_len_10_has_3",
        wi(&[BINARY_EXT, 0, 0, 0, 10, 1, 2, 3]),
    ));
    out.push((
        "bit_binary_len_10_has_1",
        wi(&[BIT_BINARY_EXT, 0, 0, 0, 10, 3, 1]),
    ));

    // Maps: a bit-31 pair count, a missing pair, a duplicate key.
    out.push(("map_pair_count_bit31", wi(&[MAP_EXT, 0x80, 0, 0, 1])));
    out.push(("map_pair_count_1_no_pairs", wi(&[MAP_EXT, 0, 0, 0, 1])));
    out.push((
        "map_duplicate_key",
        wi(&[
            MAP_EXT,
            0,
            0,
            0,
            2,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            3,
        ]),
    ));

    // Tuples arity past the element count, and past the wire bound.
    out.push(("small_tuple_255_no_elements", wi(&[SMALL_TUPLE_EXT, 255])));
    out.push((
        "small_tuple_3_has_1",
        wi(&[SMALL_TUPLE_EXT, 3, SMALL_INTEGER_EXT, 1]),
    ));
    out.push(("large_tuple_arity_2_24", wi(&[LARGE_TUPLE_EXT, 1, 0, 0, 0])));

    // Bignums: a lying byte count, and the `BIG_ARITY_MAX * 8` wire bound.
    out.push(("small_big_count_200_has_1", wi(&[SMALL_BIG_EXT, 200, 0, 1])));
    out.push((
        "large_big_count_524281",
        wi(&[LARGE_BIG_EXT, 0, 7, 255, 249, 0]),
    ));
    out.push((
        "large_big_count_65535_truncated",
        wi(&[LARGE_BIG_EXT, 0, 0, 255, 255, 0]),
    ));

    // References: id count past `MAX_REF_IDS` (5), and a legacy first id at
    // `MAX_REFERENCE` (`1 << 18`).
    out.push((
        "newer_reference_len_6",
        wi(&[NEWER_REFERENCE_EXT, 0, 6, 119, 2, b'n', b'1', 0, 0, 0, 1]),
    ));
    out.push((
        "reference_ext_first_id_at_max",
        wi(&[REFERENCE_EXT, 119, 2, b'n', b'1', 0, 4, 0, 0, 0]),
    ));
    out.push((
        "new_reference_ext_first_id_at_max",
        wi(&[NEW_REFERENCE_EXT, 0, 1, 119, 2, b'n', b'1', 0, 0, 4, 0, 0]),
    ));

    // Legacy identifier creations: a non-tiny (4) creation byte.
    out.push((
        "port_ext_creation_4",
        wi(&[PORT_EXT, 119, 2, b'n', b'1', 0, 0, 0, 1, 4]),
    ));
    out.push((
        "pid_ext_creation_4",
        wi(&[PID_EXT, 119, 2, b'n', b'1', 0, 0, 0, 1, 0, 0, 0, 1, 4]),
    ));

    // Floats: a non-finite `NEW_FLOAT_EXT` and an unparsable legacy `FLOAT_EXT`.
    out.push((
        "new_float_inf",
        wi(&[NEW_FLOAT_EXT, 0x7F, 0xF0, 0, 0, 0, 0, 0, 0]),
    ));
    let mut nan = vec![FLOAT_EXT, b'n', b'a', b'n'];
    nan.resize(1 + 31, 0);
    out.push(("float_ext_nan", wi(&nan)));

    // Native records: a field count whose definition arity overflows the header.
    out.push((
        "record_field_count_2_24",
        wi(&[RECORD_EXT, 1, 0, 0, 0, 0, 119, 1, b'm', 119, 1, b'r']),
    ));

    // An atom name longer than `MAX_ATOM_CHARS` (255 characters).
    let mut long_atom = vec![ATOM_UTF8_EXT, 1, 0];
    long_atom.resize(3 + 256, b'a');
    out.push(("atom_name_256_chars", wi(&long_atom)));

    out
}

/// Images the decoder rejects as a matter of *strictness* (trailing bytes) but the
/// oracle's `binary_to_term/1` accepts — asserted offline, never oracle-compared.
fn strictness_corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("trailing_after_nil", wi(&[106, 0])),
        ("trailing_after_small", wi(&[SMALL_INTEGER_EXT, 1, 0])),
    ]
}

#[test]
fn corrupted_length_and_count_fields_are_bad_arg() {
    let cases = targeted_corpus();
    assert!(
        cases.len() >= 25,
        "the targeted corpus shrank to {} cases",
        cases.len()
    );
    for (label, image) in &cases {
        assert_bad_arg(image, label);
    }
    for (label, image) in &strictness_corpus() {
        assert_rejected(image, label);
    }
    eprintln!(
        "[beam_codec hostile] {} targeted corruptions are a typed BadArg",
        cases.len()
    );
}

// ---------------------------------------------------------------------------
// (d) `COMPRESSED` wrapper attacks
// ---------------------------------------------------------------------------

/// `[VERSION_MAGIC][COMPRESSED_EXT][declared:4 BE][payload]`.
fn compressed_wrapper(payload: &[u8], declared: u32) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    image.extend_from_slice(&declared.to_be_bytes());
    image.extend_from_slice(payload);
    image
}

fn compressed_attacks() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    // The valid inner body: `{1, 2}` (six bytes).
    let body = [
        SMALL_TUPLE_EXT,
        2,
        SMALL_INTEGER_EXT,
        1,
        SMALL_INTEGER_EXT,
        2,
    ];

    // A wrapper header too short to be taken as a wrapper (the `80` is then an ordinary
    // — and invalid — term tag).
    out.push(("compressed_tag_only", wi(&[COMPRESSED_EXT])));
    out.push(("compressed_two_bytes", wi(&[COMPRESSED_EXT, 0, 0])));
    out.push(("compressed_size_only", wi(&[COMPRESSED_EXT, 0, 0, 0, 0])));

    // A lying declared size (too small, too large, zero, and over the 32 MiB cap).
    let zlib = compress(&body, 6).expect("zlib");
    for declared in [0u32, body.len() as u32 - 1, body.len() as u32 + 1, u32::MAX] {
        out.push((
            "compressed_wrong_declared_size",
            compressed_wrapper(&zlib, declared),
        ));
    }

    // A payload that is not a zlib stream at all, and a bare zlib header with nothing
    // behind it.
    out.push((
        "compressed_non_zlib_payload",
        wi(&[COMPRESSED_EXT, 0, 0, 0, 10, 1, 2, 3, 4, 5, 6]),
    ));
    out.push((
        "compressed_zlib_header_only",
        wi(&[COMPRESSED_EXT, 0, 0, 0, 1, 0x78, 0x9C]),
    ));

    // A valid zlib stream whose inflated body is not a term.
    let garbage = compress(&[0xC8, 0xC8, 0xC8], 6).expect("zlib");
    out.push(("compressed_non_term_body", compressed_wrapper(&garbage, 3)));
    // A valid zlib stream of an empty body, declaring size 0.
    let empty = compress(&[], 6).expect("zlib");
    out.push(("compressed_empty_body", compressed_wrapper(&empty, 0)));

    // Truncation at every offset past the 6-byte header, and corruption of the deflate
    // stream / its Adler-32 trailer.
    let image = compressed_wrapper(&zlib, body.len() as u32);
    for len in 6..image.len() {
        out.push(("compressed_truncated_stream", image[..len].to_vec()));
    }
    let mut corrupt = image.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    out.push(("compressed_corrupt_trailer", corrupt));
    let mut corrupt = image.clone();
    corrupt[8] ^= 0xFF;
    out.push(("compressed_corrupt_stream", corrupt));

    out
}

#[test]
fn compressed_wrapper_attacks_are_bad_arg() {
    let cases = compressed_attacks();
    assert!(
        cases.len() >= 12,
        "the compressed attack corpus shrank to {} cases",
        cases.len()
    );
    for (label, image) in &cases {
        assert_bad_arg(image, label);
    }
    eprintln!(
        "[beam_codec hostile] {} COMPRESSED attacks are a typed BadArg",
        cases.len()
    );
}

// ---------------------------------------------------------------------------
// Live oracle cross-check of the classification (`ok` vs `badarg`)
// ---------------------------------------------------------------------------

/// The subset cross-checked against the oracle: the oracle-decodable valid images, the
/// truncations of a few small ones, and every targeted corruption (the trailing-byte
/// strictness cases are excluded — the oracle ignores trailing bytes).
fn oracle_cases() -> Vec<(&'static str, Vec<u8>)> {
    let truncate_these = [
        "small_int",
        "tuple",
        "map_flat",
        "byte_string",
        "compressed",
    ];
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for c in valid_corpus() {
        if !c.oracle {
            continue;
        }
        if truncate_these.contains(&c.label) {
            for len in 0..c.image.len() {
                out.push((c.label, c.image[..len].to_vec()));
            }
        }
        out.push((c.label, c.image));
    }
    out.extend(targeted_corpus());
    out
}

/// `true` when `p` is an existing executable file.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec hostile] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Returns the oracle, or prints a loud skip note and returns `None`.
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("============ beam_codec hostile: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=============================================================");
            eprintln!();
            None
        }
    }
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its trimmed stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
        .trim()
        .to_string()
}

/// The `<<1,2,3>>` literal the oracle's `-eval` accepts.
fn binary_literal(bytes: &[u8]) -> String {
    let joined = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{joined}>>")
}

/// Parses the `[ok,badarg,…]` list the oracle prints.
fn parse_atom_list(text: &str) -> Vec<String> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("unexpected oracle output {text:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner.split(',').map(|s| s.trim().to_string()).collect()
}

/// Classifies each image as `binary_to_term/1` sees it: `ok` or `badarg`.
fn oracle_classify(oracle: &Path, images: &[Vec<u8>]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < images.len() {
        let mut end = start;
        let mut len = 0;
        while end < images.len() && (end == start || len + images[end].len() * 4 <= 30_000) {
            len += images[end].len() * 4;
            end += 1;
        }
        let list = images[start..end]
            .iter()
            .map(|b| binary_literal(b))
            .collect::<Vec<_>>()
            .join(",");
        let eval = format!(
            "F = fun(B) -> try binary_to_term(B), ok catch _:_ -> badarg end end, \
             io:format(\"~w~n\", [[F(B) || B <- [{list}]]])"
        );
        out.extend(parse_atom_list(&run_erl(oracle, &eval)));
        start = end;
    }
    out
}

#[test]
fn oracle_agrees_on_the_hostile_classification() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_hostile_classification") else {
        return;
    };
    let cases = oracle_cases();
    let ours: Vec<&str> = cases
        .iter()
        .map(|(_, image)| {
            if decode(image).is_ok() {
                "ok"
            } else {
                "badarg"
            }
        })
        .collect();
    let images: Vec<Vec<u8>> = cases.iter().map(|(_, image)| image.clone()).collect();
    let theirs = oracle_classify(&oracle, &images);
    assert_eq!(
        theirs.len(),
        cases.len(),
        "the oracle classified {} images for {} cases",
        theirs.len(),
        cases.len()
    );
    for (i, (label, image)) in cases.iter().enumerate() {
        assert_eq!(
            ours[i], theirs[i],
            "case `{label}` image {image:?}: ours = {}, oracle = {}",
            ours[i], theirs[i]
        );
    }
    assert!(
        ours.iter().any(|c| *c == "ok"),
        "the subset must contain a decodable image"
    );
    assert!(
        ours.iter().any(|c| *c == "badarg"),
        "the subset must contain a rejected image"
    );
    eprintln!(
        "[beam_codec hostile] the oracle agreed on all {} classifications ({} ok / {} badarg)",
        cases.len(),
        ours.iter().filter(|c| **c == "ok").count(),
        ours.iter().filter(|c| **c == "badarg").count()
    );
}
