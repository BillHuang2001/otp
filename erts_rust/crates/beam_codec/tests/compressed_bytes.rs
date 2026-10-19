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

//! **The `COMPRESSED` (`80`) boundary rule, pinned offline — no oracle needed.**
//!
//! `erlang:term_to_binary(T, [{compressed, Level}])` produces
//! `[131][80][body:4 BE][zlib stream]` **iff** the zlib stream fits into the space the
//! 5-byte wrapper itself costs. Writing `body` for the plain image's length minus its
//! `VERSION_MAGIC` byte and `Z` for the length of the RFC-1950 zlib stream over exactly
//! those `body` bytes at the requested level, the image sizes are
//!
//! * plain: `1 + body`
//! * compressed: `6 + Z` (`VERSION_MAGIC` + `COMPRESSED_EXT` + the 4-byte size)
//!
//! so the wrapper is emitted **iff `6 + Z <= 1 + body` ⟺ `Z <= body - 5`**, equality
//! landing on the COMPRESSED side (the C compresses into a `body_len - 5`-byte
//! destination and keeps the plain image only on `Z_BUF_ERROR`, which equality does not
//! raise — `external.c:2276-2298`; the `TTBEncode`/`TTBCompress` machine `:2570-2854`
//! and `parse_t2b_opts` `:1480` are the same rule). A `body < 5` can never fit the
//! wrapper and always reverts; `level == 0` never compresses at all (the C's
//! `level != 0` guard) and does not even call the compressor.
//!
//! **Everything here is oracle-free.** The boundary is *derived* — each case computes
//! `Z` from [`beam_codec::compress::compress`] over the plain body and applies the rule
//! above — never hardcoded, so the sweep also fails if the crate's deflate ever drifts
//! away from zlib 1.3.2 (`beam_deflate`, the workspace's port of the zlib vendored at
//! `erts/emulator/zlib/`), which is what makes the wrapper bytes byte-identical to the
//! oracle's. The handful of oracle byte images in
//! [`embedded_oracle_goldens`] are captured constants (OTP 29.1.1 / erts-17.1), not a
//! live comparison: this file must pass with no `erl` on `$PATH`.
//!
//! Ground truth for the whole file is the legacy C
//! `../../erts/emulator/beam/external.c` + `erl_zlib.c::erl_zlib_compress2`
//! (`deflateInit(level)` then one `deflate(Z_FINISH)`).

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::prelude::{make_atom, make_small, AtomTable, Eterm, NIL};

use beam_codec::compress::{compress, decompress, MAX_LEVEL, MIN_LEVEL};
use beam_codec::{
    decode_term, encode_term, encode_term_body_compressed, encode_term_compressed, COMPRESSED_EXT,
    VERSION_MAGIC,
};

// ---------------------------------------------------------------------------
// Oracle byte images (captured, not live)
//
// Each block comment names the Erlang expression that produced the image and the
// exact lowercase hex the oracle (OTP 29.1.1 / erts-17.1) printed. The deflate here is
// zlib 1.3.2's (see `beam_deflate`), so these are byte-for-byte reproducible offline.
// ---------------------------------------------------------------------------

/// `term_to_binary(binary:copy(<<"a">>,14))` —
/// `836d0000000e6161616161616161616161616161`. `body = 19`, `Z = 15 > 19 - 5 = 14`:
/// the wrapper would *grow* the image, so the plain image is emitted.
const A14_PLAIN: &[u8] = &[
    0x83, 0x6d, 0x00, 0x00, 0x00, 0x0e, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
    0x61, 0x61, 0x61, 0x61, 0x61,
];

/// `term_to_binary(binary:copy(<<"a">>,15))` —
/// `836d0000000f616161616161616161616161616161`. `body = 20`.
const A15_PLAIN: &[u8] = &[
    0x83, 0x6d, 0x00, 0x00, 0x00, 0x0f, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
    0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
];

/// `term_to_binary(binary:copy(<<"a">>,15), [{compressed,6}])` —
/// `835000000014789ccb656060e04f4405003700062c`. `Z = 15 == body - 5 = 15`: the
/// **equality case**, and it *is* compressed (both images are 21 bytes).
const A15_COMPRESSED: &[u8] = &[
    0x83, 0x50, 0x00, 0x00, 0x00, 0x14, 0x78, 0x9c, 0xcb, 0x65, 0x60, 0x60, 0xe0, 0x4f, 0x44, 0x05,
    0x00, 0x37, 0x00, 0x06, 0x2c,
];

/// `term_to_binary(binary:copy(<<"a">>,16))` —
/// `836d0000001061616161616161616161616161616161`. `body = 21`.
const A16_PLAIN: &[u8] = &[
    0x83, 0x6d, 0x00, 0x00, 0x00, 0x10, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
    0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
];

/// `term_to_binary(binary:copy(<<"a">>,16), [{compressed,6}])` —
/// `835000000015789ccb65606010484403003d9e068e`. `Z = 15 < body - 5 = 16`: the
/// wrapper now shrinks the image (21 vs 22 bytes).
const A16_COMPRESSED: &[u8] = &[
    0x83, 0x50, 0x00, 0x00, 0x00, 0x15, 0x78, 0x9c, 0xcb, 0x65, 0x60, 0x60, 0x10, 0x48, 0x44, 0x03,
    0x00, 0x3d, 0x9e, 0x06, 0x8e,
];

/// `term_to_binary(binary:copy(<<"a">>,1024), [{compressed,6}])` —
/// `835000000405789ccb65606061481c05a360148c580000b9c48481` (`body = 1029`,
/// `Z = 21`).
const A1024_COMPRESSED: &[u8] = &[
    0x83, 0x50, 0x00, 0x00, 0x04, 0x05, 0x78, 0x9c, 0xcb, 0x65, 0x60, 0x60, 0x61, 0x48, 0x1c, 0x05,
    0xa3, 0x60, 0x14, 0x8c, 0x58, 0x00, 0x00, 0xb9, 0xc4, 0x84, 0x81,
];

/// `term_to_binary(ok)` — `8377026f6b`. `body = 4 < 5`, so no wrapper can ever fit.
const OK_PLAIN: &[u8] = &[0x83, 0x77, 0x02, 0x6f, 0x6b];

/// `term_to_binary(1)` — `836101` (`body = 2`).
const ONE_PLAIN: &[u8] = &[0x83, 0x61, 0x01];

/// `term_to_binary([])` — `836a` (`body = 1`).
const NIL_PLAIN: &[u8] = &[0x83, 0x6a];

/// `term_to_binary(<<>>)` — `836d00000000` (`body = 5`, `Z = 11 > 0`, so plain).
const EMPTY_BINARY_PLAIN: &[u8] = &[0x83, 0x6d, 0x00, 0x00, 0x00, 0x00];

/// `term_to_binary({1,2,3})` — `836803610161026103` (`body = 8`, `Z = 13 > 3`, plain).
const TUPLE123_PLAIN: &[u8] = &[0x83, 0x68, 0x03, 0x61, 0x01, 0x61, 0x02, 0x61, 0x03];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// A heap with room for an on-heap `ErlHeapBits` block holding `bytes` bytes
/// (`heap_bits_size_words` is `2 + ceil(bytes / 8)` words) plus slack.
fn heap_for_bytes(bytes: usize) -> Heap {
    heap(bytes / 8 + 128)
}

/// The term `Bytes` — an on-heap binary — plus the (empty) atom table it needs.
fn binary_term(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut h = heap_for_bytes(bytes.len());
    let t = beam_codec::term::make_binary(&mut h, bytes).expect("an on-heap binary");
    (h, AtomTable::new(), t)
}

/// `lists:duplicate(N, Name)` — a proper list of `N` copies of one atom.
fn atom_list(atom_name: &str, n: usize) -> (Heap, AtomTable, Eterm) {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern(atom_name));
    let mut h = heap(2 * n + 64);
    let mut acc = NIL;
    for _ in 0..n {
        acc = h.cons(a, acc).expect("heap sized for the list");
    }
    (h, atoms, acc)
}

fn is_compressed(image: &[u8]) -> bool {
    image.len() >= 6 && image[0] == VERSION_MAGIC && image[1] == COMPRESSED_EXT
}

/// The 4-byte declared inner size of a compressed image.
fn declared_size(image: &[u8]) -> usize {
    assert!(image.len() >= 6, "truncated compressed header: {image:?}");
    u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize
}

fn assert_bad_arg<T: core::fmt::Debug>(result: beam_common::Result<T>) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BeamError::BadArg, got {other:?}"),
    }
}

/// Which image the C rule selects for a `body_len`-byte body deflating to `z` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Compressed,
    Plain,
}

impl Verdict {
    fn of(image: &[u8]) -> Verdict {
        if is_compressed(image) {
            Verdict::Compressed
        } else {
            Verdict::Plain
        }
    }
}

/// **The rule, derived rather than hardcoded:** the wrapper is `6 + Z` bytes against
/// the plain image's `1 + body`, so emit it iff `Z <= body - 5`; a `body < 5` can never
/// satisfy that (`checked_sub` → `None`).
fn derived_verdict(body_len: usize, z: usize) -> Verdict {
    match body_len.checked_sub(5) {
        Some(cap) if z <= cap => Verdict::Compressed,
        _ => Verdict::Plain,
    }
}

/// One point of a derived sweep: the term size, the body length, the zlib stream
/// length, whether `Z == body - 5` (the equality case) and what the encoder did.
#[derive(Debug)]
struct Point {
    n: usize,
    body: usize,
    z: usize,
    equality: bool,
    verdict: Verdict,
}

/// Builds a term, encodes it plain and compressed at `level`, checks the *derived*
/// verdict against the actual image (including the exact wrapper bytes when
/// compressed), and returns the observation.
fn observe<F>(n: usize, bytes: &[u8], build: F, level: u8) -> Point
where
    F: FnOnce(&[u8]) -> (Heap, AtomTable, Eterm),
{
    let (_h, atoms, term) = build(bytes);
    let plain = encode_term(term, &atoms).expect("the plain image encodes");
    let body = plain
        .len()
        .checked_sub(1)
        .expect("a plain image carries its version byte");
    assert_eq!(plain[0], VERSION_MAGIC, "every plain image starts with 131");
    let stream = compress(&plain[1..], level).expect("zlib accepts level 0..=9");
    let expected = derived_verdict(body, stream.len());
    let image = encode_term_compressed(term, &atoms, level).expect("the compressed image encodes");
    let verdict = Verdict::of(&image);

    assert_eq!(
        verdict, expected,
        "the encoder must follow Z <= body - 5: n={n} body={body} z={} image={image:02x?}",
        stream.len()
    );
    match expected {
        Verdict::Compressed => {
            assert_eq!(
                image[2..6],
                u32::try_from(body)
                    .expect("the sweep's bodies fit the 4-byte size field")
                    .to_be_bytes()[..],
                "the declared inner size is the body length, version byte excluded"
            );
            assert_eq!(
                image[6..],
                stream[..],
                "the wrapper carries exactly the zlib stream of the plain body"
            );
            assert_eq!(image.len(), 6 + stream.len());
        }
        Verdict::Plain => {
            assert_eq!(
                image, plain,
                "a reverted image must be the plain image verbatim"
            );
        }
    }

    Point {
        n,
        body,
        z: stream.len(),
        equality: body.checked_sub(5) == Some(stream.len()),
        verdict,
    }
}

// ---------------------------------------------------------------------------
// 1. The embedded oracle goldens
// ---------------------------------------------------------------------------

/// The oracle's exact bytes for a spread of terms, all reproduced offline.
///
/// Covers both sides of the boundary (`<<a*14>>` reverted, `<<a*15>>` the equality
/// case, `<<a*16>>` shrunk, `<<a*1024>>` strongly shrunk) and the small terms whose
/// body is shorter than the 5-byte wrapper's own cost (`ok`, `1`, `[]`, `<<>>`,
/// `{1,2,3}`).
#[test]
fn embedded_oracle_goldens() {
    // binary:copy(<<"a">>,14) — plain (Z=15 > body-5=14, reverted).
    let (_h, atoms, term) = binary_term(&[b'a'; 14]);
    assert_eq!(encode_term(term, &atoms).expect("plain"), A14_PLAIN);
    assert_eq!(
        encode_term_compressed(term, &atoms, 6).expect("compressed"),
        A14_PLAIN,
        "binary:copy(<<\"a\">>,14) [compressed] must revert to the plain image"
    );

    // binary:copy(<<"a">>,15) — the EQUALITY case, compressed (Z=15 == body-5=15).
    let (_h, atoms, term) = binary_term(&[b'a'; 15]);
    assert_eq!(encode_term(term, &atoms).expect("plain"), A15_PLAIN);
    assert_eq!(
        encode_term_compressed(term, &atoms, 6).expect("compressed"),
        A15_COMPRESSED,
        "binary:copy(<<\"a\">>,15) [compressed] must be the wrapper (equality is compressed)"
    );

    // binary:copy(<<"a">>,16) — compressed (Z=15 < body-5=16).
    let (_h, atoms, term) = binary_term(&[b'a'; 16]);
    assert_eq!(encode_term(term, &atoms).expect("plain"), A16_PLAIN);
    assert_eq!(
        encode_term_compressed(term, &atoms, 6).expect("compressed"),
        A16_COMPRESSED
    );

    // binary:copy(<<"a">>,1024) — compressed (Z=21 << body-5=1024).
    let (_h, atoms, term) = binary_term(&[b'a'; 1024]);
    let mut plain_a1024 = vec![VERSION_MAGIC, 0x6d, 0x00, 0x00, 0x04, 0x00];
    plain_a1024.extend_from_slice(&[b'a'; 1024]);
    assert_eq!(encode_term(term, &atoms).expect("plain"), plain_a1024);
    assert_eq!(
        encode_term_compressed(term, &atoms, 6).expect("compressed"),
        A1024_COMPRESSED
    );

    // ok — body 4 < 5: the wrapper can never fit, so plain.
    let mut ok_atoms = AtomTable::new();
    let ok = make_atom(ok_atoms.intern("ok"));
    assert_eq!(encode_term(ok, &ok_atoms).expect("plain"), OK_PLAIN);
    assert_eq!(
        encode_term_compressed(ok, &ok_atoms, 6).expect("compressed"),
        OK_PLAIN,
        "ok [compressed] must revert to the plain image"
    );

    // 1 — body 2.
    let empty_atoms = AtomTable::new();
    let one = make_small(1);
    assert_eq!(encode_term(one, &empty_atoms).expect("plain"), ONE_PLAIN);
    assert_eq!(
        encode_term_compressed(one, &empty_atoms, 6).expect("compressed"),
        ONE_PLAIN
    );

    // [] — body 1.
    assert_eq!(encode_term(NIL, &empty_atoms).expect("plain"), NIL_PLAIN);
    assert_eq!(
        encode_term_compressed(NIL, &empty_atoms, 6).expect("compressed"),
        NIL_PLAIN
    );

    // <<>> — body 5, and Z=11 > 0, so plain.
    let (_h, atoms, empty) = binary_term(&[]);
    assert_eq!(
        encode_term(empty, &atoms).expect("plain"),
        EMPTY_BINARY_PLAIN
    );
    assert_eq!(
        encode_term_compressed(empty, &atoms, 6).expect("compressed"),
        EMPTY_BINARY_PLAIN
    );

    // {1,2,3} — body 8, Z=13 > 3, so plain.
    let mut h = heap(64);
    let tuple = h
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("a 3-tuple");
    assert_eq!(encode_term(tuple, &empty_atoms).expect("plain"), TUPLE123_PLAIN);
    assert_eq!(
        encode_term_compressed(tuple, &empty_atoms, 6).expect("compressed"),
        TUPLE123_PLAIN
    );
}

// ---------------------------------------------------------------------------
// 2. The derived boundary sweep
// ---------------------------------------------------------------------------

/// **The boundary is exactly `Z <= body - 5`**, derived over a sweep of
/// `binary:copy(<<"a">>, N)` for `N` in `0..=64`.
///
/// Nothing about the crossing is hardcoded: each `N` recomputes `Z` from the crate's
/// zlib over the plain body and applies the rule; the crossing point, the equality case
/// and its side of the boundary are then *read off* the sweep.
#[test]
fn the_boundary_is_exactly_z_le_body_minus_five() {
    let sweep: Vec<Point> = (0..=64usize)
        .map(|n| {
            let bytes = vec![b'a'; n];
            observe(n, &bytes, binary_term, 6)
        })
        .collect();

    for point in &sweep {
        if point.body < 5 {
            assert_eq!(
                point.verdict,
                Verdict::Plain,
                "a body shorter than the wrapper always reverts: {point:?}"
            );
        }
    }

    // Exactly one N is the smallest compressed one, and it is the smallest N that
    // satisfies the derived rule at all.
    let smallest_compressed = sweep
        .iter()
        .find(|p| p.verdict == Verdict::Compressed)
        .expect("some N in 0..=64 must satisfy Z <= body - 5");
    let smallest_satisfying = sweep
        .iter()
        .find(|p| derived_verdict(p.body, p.z) == Verdict::Compressed)
        .expect("the rule must select at least one N");
    assert_eq!(
        smallest_compressed.n, smallest_satisfying.n,
        "the first compressed N must be the first N satisfying the rule"
    );

    // The equality case exists, is unique in the sweep, and came out COMPRESSED.
    let equalities: Vec<&Point> = sweep.iter().filter(|p| p.equality).collect();
    assert!(
        !equalities.is_empty(),
        "the sweep must include at least one Z == body - 5 case: {sweep:?}"
    );
    for point in &equalities {
        assert_eq!(
            point.verdict,
            Verdict::Compressed,
            "equality lands on the COMPRESSED side: {point:?}"
        );
    }

    // Documented observation: the crossing is N = 15 (body 20, Z 15), which *is* the
    // equality case — i.e. the very first size that satisfies the rule satisfies it
    // with equality.
    assert_eq!(
        smallest_compressed.n, 15,
        "observed crossing: {smallest_compressed:?}"
    );
    assert_eq!(
        smallest_compressed.body, 20,
        "observed crossing body: {smallest_compressed:?}"
    );
    assert_eq!(
        smallest_compressed.z, 15,
        "observed crossing stream length: {smallest_compressed:?}"
    );
    assert!(
        smallest_compressed.equality,
        "the observed crossing must be the equality case: {smallest_compressed:?}"
    );
    assert_eq!(
        equalities.len(),
        1,
        "exactly one N in 0..=64 is the equality case: {equalities:?}"
    );
    assert_eq!(
        equalities[0].n, smallest_compressed.n,
        "the sweep's equality case must be its crossing point"
    );

    // And the neighbour just below the crossing is plain, one byte short of the cap.
    let below = &sweep[14];
    assert_eq!(below.n, 14);
    assert_eq!(below.verdict, Verdict::Plain);
    assert_eq!(
        below.z,
        below.body - 5 + 1,
        "N=14 misses the cap by exactly one byte: {below:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Level 0 is never compressed
// ---------------------------------------------------------------------------

/// `{compressed, 0}` never compresses: the C's `level != 0` guard means the compressor
/// is not even called, so the result is the plain image verbatim — for a highly
/// compressible binary *and* for a large list, and for both `body`-form entry points'
/// shared encoder.
#[test]
fn level_zero_is_never_compressed() {
    assert_eq!(MIN_LEVEL, 0, "level 0 is the accepted minimum");
    assert_eq!(MAX_LEVEL, 9, "level 9 is the accepted maximum");

    // A 4096-byte binary compresses by ~200x at every level >= 1, yet must not compress
    // at level 0.
    let (_h, atoms, term) = binary_term(&[b'a'; 4096]);
    let plain = encode_term(term, &atoms).expect("plain");
    assert_eq!(
        encode_term_compressed(term, &atoms, 0).expect("level 0"),
        plain,
        "level 0 must yield the plain image byte for byte"
    );
    let zero_image = encode_term_compressed(term, &atoms, 0).expect("level 0");
    assert!(!is_compressed(&zero_image), "level 0 image: {zero_image:02x?}");
    assert_eq!(zero_image[1], 0x6d, "a binary's plain image uses 109");

    // ... and a 500-element list, same story.
    let (_h, atoms, list) = atom_list("hello", 500);
    let list_plain = encode_term(list, &atoms).expect("plain");
    assert_eq!(
        encode_term_compressed(list, &atoms, 0).expect("level 0"),
        list_plain,
        "level 0 must yield the plain list image byte for byte"
    );
    assert!(!is_compressed(
        &encode_term_compressed(list, &atoms, 0).expect("level 0")
    ));

    // Every level 1..=9 *does* produce the wrapper for these terms (they compress far
    // below body - 5), and every such image inflates back to the plain body.
    for level in 1..=MAX_LEVEL {
        let image = encode_term_compressed(term, &atoms, level).expect("compressed");
        assert!(
            is_compressed(&image),
            "level {level} must compress a 4096-byte binary: {image:02x?}"
        );
        assert_eq!(decompress(&image[6..]).expect("inflate"), plain[1..]);

        let image = encode_term_compressed(list, &atoms, level).expect("compressed");
        assert!(
            is_compressed(&image),
            "level {level} must compress a 500-element list: {image:02x?}"
        );
        assert_eq!(decompress(&image[6..]).expect("inflate"), list_plain[1..]);
    }

    // The raw helper still accepts level 0 — it is only the ETF path that never calls
    // it at 0 (the stream's block boundaries there are pump-schedule dependent).
    let body = &plain[1..];
    let stored = compress(body, MIN_LEVEL).expect("the raw helper compresses at level 0");
    assert!(stored.len() > body.len(), "a stored stream is larger: {stored:02x?}");
    assert_eq!(decompress(&stored).expect("inflate"), *body);
}

// ---------------------------------------------------------------------------
// 4. Out-of-range levels
// ---------------------------------------------------------------------------

/// A level outside `0..=9` is `badarg` — on the encoder and on the raw helper alike —
/// while the two ends of the accepted range are fine.
#[test]
fn out_of_range_levels_are_badarg() {
    let (_h, atoms, term) = binary_term(&[b'a'; 64]);

    assert_bad_arg(encode_term_compressed(term, &atoms, MAX_LEVEL + 1));
    assert_bad_arg(encode_term_compressed(term, &atoms, 255));
    assert_bad_arg(encode_term_body_compressed(term, &atoms, MAX_LEVEL + 1));
    assert_bad_arg(encode_term_body_compressed(term, &atoms, 255));

    assert_bad_arg(compress(&[], MAX_LEVEL + 1));
    assert_bad_arg(compress(&[], 255));
    // ... and the ends of the accepted range are not.
    assert!(compress(&[], MIN_LEVEL).is_ok());
    assert!(compress(&[], MAX_LEVEL).is_ok());
    assert!(encode_term_compressed(term, &atoms, MIN_LEVEL).is_ok());
    assert!(encode_term_compressed(term, &atoms, MAX_LEVEL).is_ok());
}

// ---------------------------------------------------------------------------
// 5. The C's second code path
// ---------------------------------------------------------------------------

/// The `band 16#7FFFFFFF` mask the LCG's state word is folded with.
const LCG_MASK: u64 = 0x7FFF_FFFF;

/// `N` pseudo-random (incompressible) bytes.
///
/// The LCG is `s_{n+1} = (s_n*1103515245 + 12345) band 16#7FFFFFFF` with
/// `byte = (s_{n+1} bsr 16) band 255` and `s_0 = 12345`, so the first byte is drawn
/// from `s_1`.
fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut s: u64 = 12345;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        s = (s * 1103515245 + 12345) & LCG_MASK;
        out.push(((s >> 16) & 0xFF) as u8);
    }
    out
}

/// `<<zeros(P), Lcg300>>` — the second-code-path term: a binary whose ETF body is
/// `[109][len:4 BE][zeros(P)][lcg bytes]`, i.e. `305 + P` bytes, well past the 64-byte
/// boundary where the C's `erts_term_to_binary_simple` takes its second path.
fn zeros_then_lcg(p: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; p];
    bytes.extend_from_slice(&lcg_bytes(300));
    bytes
}

/// The C's second code path (`external.c:2248-2321`) accepts the equality case too, so
/// the shrink rule is path-independent: deriving the verdict from `Z <= body - 5` for
/// `P` in `28..=36` reproduces the oracle's crossing at `P = 33`, which is an equality
/// case and *is* compressed, while `P = 32` is plain.
#[test]
fn second_path_boundary() {
    let mut sweep: Vec<Point> = Vec::new();
    for p in 28..=36usize {
        let bytes = zeros_then_lcg(p);
        let point = observe(p, &bytes, binary_term, 6);
        assert!(
            point.body > 64,
            "the second code path is the >64-byte one: {point:?}"
        );
        sweep.push(point);
    }

    let crossing = sweep
        .iter()
        .find(|p| p.verdict == Verdict::Compressed)
        .expect("some P in 28..=36 must satisfy the rule");
    // Documented observation: the oracle's crossing is P = 33 ...
    assert_eq!(crossing.n, 33, "observed second-path crossing: {crossing:?}");
    assert_eq!(crossing.body, 338, "crossing body: {crossing:?}");
    assert_eq!(crossing.z, 333, "crossing stream length: {crossing:?}");
    // ... and it is an equality case.
    assert!(
        crossing.equality,
        "the second-path crossing must be the equality case: {crossing:?}"
    );
    let equalities: Vec<&Point> = sweep.iter().filter(|p| p.equality).collect();
    assert_eq!(equalities.len(), 1, "one equality case in 28..=36: {equalities:?}");
    assert_eq!(equalities[0].n, 33);
    assert_eq!(equalities[0].verdict, Verdict::Compressed);
    // P=32 is one byte short and stays plain; P=33 is the equality case and compresses.
    let p32 = &sweep[32 - 28];
    assert_eq!(p32.n, 32);
    assert_eq!(p32.verdict, Verdict::Plain, "P=32 must be plain: {p32:?}");
    assert_eq!(
        p32.z,
        p32.body - 5 + 1,
        "P=32 misses the cap by exactly one byte: {p32:?}"
    );
    let p33 = &sweep[33 - 28];
    assert_eq!(p33.verdict, Verdict::Compressed, "P=33 must compress: {p33:?}");
    assert!(p33.equality);

    // Incompressible bodies stay plain — derived, and the plain image is returned
    // unchanged.
    for n in [1024usize, 10240] {
        let bytes = lcg_bytes(n);
        let point = observe(n, &bytes, binary_term, 6);
        assert!(
            point.z > point.body,
            "{n} pseudo-random bytes cannot compress: {point:?}"
        );
        assert_eq!(
            point.verdict,
            Verdict::Plain,
            "{n} pseudo-random bytes must stay plain: {point:?}"
        );

        // ... and "plain" means the *unchanged* plain image, not a bespoke revert.
        let (_h, atoms, term) = binary_term(&bytes);
        let plain = encode_term(term, &atoms).expect("plain");
        assert_eq!(
            encode_term_compressed(term, &atoms, 6).expect("compressed"),
            plain,
            "{n} pseudo-random bytes: the plain image must be returned unchanged"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Decode -> re-encode is a fixed point
// ---------------------------------------------------------------------------

/// A compressed image decodes here and re-encodes to **the same bytes**, which is what
/// lets the round-trip suites compare compressed images directly: the decoder
/// materialises the term, and the encoder re-deflates the same body with the same zlib —
/// so the wrapper *and* the stream are reproduced exactly.
#[test]
fn decode_then_reencode_is_byte_identical() {
    for (label, golden, bytes) in [
        ("binary:copy(<<\"a\">>,1024)", A1024_COMPRESSED, vec![b'a'; 1024]),
        ("binary:copy(<<\"a\">>,16)", A16_COMPRESSED, vec![b'a'; 16]),
        ("binary:copy(<<\"a\">>,15)", A15_COMPRESSED, vec![b'a'; 15]),
    ] {
        let mut h = heap_for_bytes(bytes.len());
        let mut atoms = AtomTable::new();
        let decoded = decode_term(golden, &mut h, &mut atoms).expect("the golden decodes");
        let reencoded =
            encode_term_compressed(decoded, &atoms, 6).expect("the decoded term re-encodes");
        assert_eq!(
            reencoded, golden,
            "{label}: decode -> re-encode must be a fixed point\n  \
             golden   = {golden:02x?}\n  reencoded = {reencoded:02x?}"
        );
        // The plain image of the decoded term is the wrapper's inner body, too.
        assert_eq!(
            declared_size(&reencoded),
            encode_term(decoded, &atoms).expect("plain").len() - 1
        );
    }

    // A reverted (plain) image is a fixed point at the image level as well.
    let (_h, atoms, term) = binary_term(&[b'a'; 14]);
    let plain = encode_term_compressed(term, &atoms, 6).expect("reverted");
    let mut h = heap_for_bytes(64);
    let mut atoms2 = AtomTable::new();
    let decoded = decode_term(&plain, &mut h, &mut atoms2).expect("the plain image decodes");
    assert_eq!(
        encode_term_compressed(decoded, &atoms2, 6).expect("re-encode"),
        plain
    );
}

// ---------------------------------------------------------------------------
// 7. The body form
// ---------------------------------------------------------------------------

/// `encode_term_body_compressed` is `encode_term_compressed` minus the version byte for
/// a compressed term *and* for a reverted one, so the `Encoder::new` and
/// `Encoder::new_body` routes agree on the wrapper.
#[test]
fn body_form_matches_the_image_minus_the_version_byte() {
    // Compressed.
    let (_h, atoms, big) = binary_term(&[b'a'; 1024]);
    let image = encode_term_compressed(big, &atoms, 6).expect("compressed");
    assert!(is_compressed(&image));
    assert_eq!(
        encode_term_body_compressed(big, &atoms, 6).expect("body form"),
        image[1..],
        "the body form is the image with the version byte stripped"
    );
    // The body form starts with the tag, then the declared size, then the stream.
    let body_form = encode_term_body_compressed(big, &atoms, 6).expect("body form");
    assert_eq!(body_form[0], COMPRESSED_EXT);
    assert_eq!(
        u32::from_be_bytes([body_form[1], body_form[2], body_form[3], body_form[4]]) as usize,
        declared_size(&image),
        "the body form declares the same inner size"
    );
    assert_eq!(body_form[5..], image[6..], "the body form carries the same stream");

    // Reverted (plain).
    let (_h, atoms, small) = binary_term(&[b'a'; 14]);
    let image = encode_term_compressed(small, &atoms, 6).expect("reverted");
    assert!(!is_compressed(&image));
    assert_eq!(
        encode_term_body_compressed(small, &atoms, 6).expect("body form"),
        image[1..],
        "a reverted body form is the plain body"
    );
    assert_eq!(
        encode_term_body_compressed(small, &atoms, 6).expect("body form"),
        encode_term(small, &atoms).expect("plain")[1..]
    );
}
