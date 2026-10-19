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
//! **The crate boundary** — `src/lib.rs`'s one-shot free functions, tested the way the two
//! consumers will call them.
//!
//! Two consumers are being proved here, and they are the only reason this crate exists:
//!
//! * **`beam_codec` — the ETF `COMPRESSED` payload.** `term_to_binary(T, [compressed])` is
//!   `[131][80][InnerSize:4 BE][zlib stream]`, where the stream is exactly
//!   `erl_zlib_compress2`'s (`deflateInit(level)` → `deflate(Z_FINISH)` → `deflateEnd`).
//!   The encoder therefore needs [`compress(data, level)`] — level `-1` for `[compressed]`,
//!   `0..=9` for `[{compressed, Level}]` (`erts/emulator/beam/external.c::parse_t2b_opts`) —
//!   and the decoder needs [`decompress_bounded_used(data, limit)`], whose count is
//!   `binary_to_term/2`'s `used`: the deflate data plus its trailer, **excluding** whatever
//!   follows the stream.
//! * **`beam_bifs` — the `zlib` BIF layer's one-shot forms.** `zlib:compress/1` is
//!   [`compress`] at [`DEFAULT_COMPRESSION`] and `zlib:uncompress/1` is [`decompress`].
//!
//! The streaming (pump-level) contract — the loop `zlib_nif.c` drives — is the sibling
//! `tests/roundtrip.rs`; the per-framing byte goldens are `tests/deflate_goldens.rs` and
//! `tests/inflate_goldens.rs`.
//!
//! # What is pinned
//!
//! * **Byte identity with the oracle**, twice: against embedded literals (so the claim
//!   holds without an Erlang installation) and against the live `erl`'s `zlib` module when
//!   it is on `PATH` (`SKIP …: erl not found` when it is not).
//! * **The level surface**: `-1` *is* level 6 (zlib's `Z_DEFAULT_COMPRESSION` sentinel),
//!   every level `0..=9` is accepted, and everything else is
//!   [`beam_common::BeamError::BadArg`] — the C's `deflateInit2_` `Z_STREAM_ERROR`.
//! * **The bounded-decode contract**: the `used` count, the inclusive `limit`, the
//!   `decompress`/`decompress_bounded`/`decompress_bounded_used` agreement, the
//!   `usize::MAX` (unbounded) path on a multi-MiB payload, and that every malformed shape
//!   — empty input, a lone header byte, a header with no body, a corrupt Adler-32 trailer,
//!   and *every* truncation prefix — is `BadArg` and never a panic.
//! * **Round trips** over a deterministic corpus (ten sizes × four generators × eleven
//!   levels): `decompress(compress(x, l)) == x`.
//!
//! # Regenerating the embedded images
//!
//! Only when the oracle itself changes:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_deflate --test lib_api -- --ignored --nocapture
//! ```
//!
//! That prints both tables between `=== NAME ===` markers (asserting first that this port
//! already produces the oracle's bytes), ready to paste back.

mod zsupport;

use beam_common::BeamError;
use beam_deflate::{
    compress, decompress, decompress_bounded, decompress_bounded_used, DEFAULT_COMPRESSION,
};
use zsupport::hex;

/// The oracle's `zlib:compress/1` image — `deflateInit(Z, default)` +
/// `deflate(Z, Data, finish)` — of each [`zsupport::corpus_small`] input, in that order:
/// empty, `abc`, 16 zeros, 300 `'a'`s, [`zsupport::mixed_kib`].
// Every entry keeps the `concat!` wrapper (even the one-chunk ones) so the whole table can
// be pasted back from the golden regenerator below verbatim.
#[allow(clippy::useless_concat)]
const DEFAULT_COMPRESSION_IMAGES: [&str; 5] = [    // empty
    concat!("789c030000000001",),
    // abc
    concat!("789c4b4c4a0600024d0127",),
    // zeros16
    concat!("789c636040050000100001",),
    // a300
    concat!("789c4b4c1c05c40200d8a871ad",),
    // mixed (mixed_kib)
    concat!(
        "789c7d57d962c33008f3a7f4d776dfcbdaadbbbe7e23b64112240f6d1c1f984308723c3f5c3d1d5a6bd7cb9d3d96",
        "cf9b933d1fcf2f6fef87fe7f5cf7dc2edfb00fd73fee6f0e63da36f5c929ebf9e2f767ddd1a5d8689dfa5fbf3c2d",
        "5faf21cc674cc871ea3515f2893ef0d77e648a744973c2ee835713bd8efb3197e2e7fa4c5fed93f328ae0c3fa02a",
        "6ed8dc2f0ef5f56dcbfa7de2956180982f82dd0feb64df925c82269b7223a0707068bd8ed7bf889b6bc5eef0f3b6",
        "9304e2cdb6c80274eb784fc8c0cbe10a56c226115736661c633c6d95141a811a7e8cb84e11c9620010d91f42e6d1",
        "a12e3922b20272669eb129fbed80ce65d8a04b9ed7a93ba7a9ac1cdb01698d49d2940ac2518172db588630109466",
        "f12e4424124a0a0a64a2c01e2c45b5c514496f7595bb7e0ac7f8414a0da5ece1122102680e1d4808ea87c87d207b",
        "5cacb888d5adb441b4f47f86987331510b1848c661ea6492002f4dd07248301bd114571e12ae8c8c20696303f81f",
        "bd4d40827b4d93a96fabb82a875b081da813934610c7d5d2cb8ebdfb9d9cb6513a5d174501240d5666090b3c0804",
        "8c8554012093b6213b36047e2a2247e0701582ac76dfb95fd4939262fb19453d0a1374452539f451c5d5a420f8cc",
        "cc723fc626fc2f0941241315b6d40d3914d020f5116780eb550fbd1ba03ee486139ab602686fa65319d4648eac4a",
        "8610e05baaa2985a5009b2ed5c764a3a886086e14c2f5260c2255a3e05206411575cea991b13e774861212a40f89",
        "c43225ca175ca99ecded5069115526d99db0a4d605e96023464fac9439ba2e83299bc98ef88882a0895fdcdc52b3",
        "adc155a39024e468492498b8c47d5ea9a2071507ef740b5c61b73e89a8bb95662e17bce007f88e2b3e7330988927",
        "8acea2e86e45d5ad52137792f14dab6a426288a594c48cde4900e267004ba8b35d14215bc5aad4bbfb9ce48f9ccb",
        "69c19fa3dc05151f6a55d3e425c703fe0735f98dbb",
    ),
];

/// The oracle's image of `b"abc"` at levels `0..=9`, in order — the whole
/// `deflateInit(Z, Level)` × `deflate(Z_FINISH)` surface on the smallest non-empty input.
///
/// Note that levels 2-5 share one image with level 1 (the `deflate_fast` path and the same
/// header flags) and 8-9 share level 7's: at three bytes there is nothing for the level
/// parameters to change but the header's `level_flags`.
// Pasted back from the golden regenerator verbatim, `concat!` wrapper included.
#[allow(clippy::useless_concat)]
const ABC_LEVEL_IMAGES: [&str; 10] = [    // level 0
    concat!("7801010300fcff616263024d0127",),
    // level 1
    concat!("78014b4c4a0600024d0127",),
    // level 2
    concat!("785e4b4c4a0600024d0127",),
    // level 3
    concat!("785e4b4c4a0600024d0127",),
    // level 4
    concat!("785e4b4c4a0600024d0127",),
    // level 5
    concat!("785e4b4c4a0600024d0127",),
    // level 6
    concat!("789c4b4c4a0600024d0127",),
    // level 7
    concat!("78da4b4c4a0600024d0127",),
    // level 8
    concat!("78da4b4c4a0600024d0127",),
    // level 9
    concat!("78da4b4c4a0600024d0127",),
];

/// Every level [`compress`] accepts, the `Z_DEFAULT_COMPRESSION` sentinel first.
const LEVELS: [i8; 11] = [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

/// The payload of the `a300` corpus entry — the one the `limit` contract is stated on,
/// because 300 is big enough to bound from both sides and small enough to inline.
const A300_LEN: usize = 300;

/// The `(byte offset, bit mask)` positions of the [`a300_image`] that a decoder never
/// reads: the padding after the final end-of-block code plus code space this 300-byte
/// payload does not use. The oracle's zlib reports the same six (bit 7 of byte 5, bits 4-6
/// of byte 7, bits 6-7 of byte 8), so pinning them also pins that this port decodes exactly
/// what zlib decodes.
const UNREAD_BIT_POSITIONS: [(usize, u8); 6] = [
    (5, 0x80),
    (7, 0x10),
    (7, 0x20),
    (7, 0x40),
    (8, 0x40),
    (8, 0x80),
];

/// The oracle's image of 300 `'a'`s (the `a300` corpus entry).
fn a300_image() -> Vec<u8> {
    zsupport::unhex(DEFAULT_COMPRESSION_IMAGES[3])
}

// ---------------------------------------------------------------------------
// The deterministic round-trip corpus
// ---------------------------------------------------------------------------

/// A 64-bit LCG in `[0, 2^32)`, so the corpus is identical on every host and every run
/// (no `rand`, no clock). The low byte of an LCG is its weakest bit, but that only makes
/// the stream less compressible — which is what this generator is for.
fn lcg_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push((state >> 33) as u8);
    }
    out
}

/// `n` bytes that repeat an 8-byte pattern — the maximally compressible case, where every
/// level but 0 emits a long run of matches.
fn repetitive(n: usize) -> Vec<u8> {
    const PATTERN: &[u8] = b"beam-gen";
    (0..n).map(|i| PATTERN[i % PATTERN.len()]).collect()
}

/// The corpus: `(name, generator)` for each shape the round trip must survive. `lcg` is
/// effectively incompressible (stored blocks), `repetitive` is pure matches, and the two
/// constant runs sit at the extremes of the literal/run encodings.
type Generator = (&'static str, fn(usize) -> Vec<u8>);

/// The four corpus shapes.
fn generators() -> [Generator; 4] {    [
        ("repetitive", repetitive),
        ("lcg", |n| lcg_bytes(n, 0x5eed)),
        ("zeros", |n| vec![0u8; n]),
        ("ones", |n| vec![0xffu8; n]),
    ]
}

/// The sizes the round trip covers: the empty payload, the sub-`MIN_MATCH` lengths, the
/// on-heap binary limit, a page, and 64 KiB (past the 32 KiB window, so the history slides).
const SIZES: [usize; 10] = [0, 1, 2, 3, 7, 255, 256, 512, 4096, 65536];

// ---------------------------------------------------------------------------
// `compress` — byte identity and the level surface
// ---------------------------------------------------------------------------

/// `Z_DEFAULT_COMPRESSION` reproduces `zlib:compress/1`'s image for the whole small corpus.
#[test]
fn the_default_level_matches_the_oracle_images() {
    for (index, ((name, data), image)) in zsupport::corpus_small()
        .iter()
        .zip(DEFAULT_COMPRESSION_IMAGES)
        .enumerate()
    {
        let ours = compress(data, DEFAULT_COMPRESSION)
            .unwrap_or_else(|e| panic!("{name}: compress failed ({e:?})"));
        assert_eq!(
            hex(&ours),
            image,
            "case {index} ({name}): not `zlib:compress/1`'s image"
        );
        assert_eq!(ours, zsupport::unhex(image), "case {index} ({name})");
    }
}

/// Every level `0..=9` reproduces the oracle's `deflate(Z_FINISH)` image, and the `-1`
/// sentinel is exactly level 6 (zlib resolves it through `configuration_table[6]`).
#[test]
fn every_level_matches_the_oracle_images() {
    for (level, image) in ABC_LEVEL_IMAGES.iter().enumerate() {
        let ours = compress(b"abc", level as i8).expect("an in-range level");
        assert_eq!(hex(&ours), *image, "level {level}");
    }
    assert_eq!(
        compress(b"abc", DEFAULT_COMPRESSION).expect("default"),
        compress(b"abc", 6).expect("level 6"),
        "-1 must resolve to level 6"
    );
    assert_eq!(
        ABC_LEVEL_IMAGES[6], DEFAULT_COMPRESSION_IMAGES[1],
        "the two tables must agree at level 6 (the `abc` input)"
    );
}

/// The same claim, derived from the live oracle: one `erl` boot for the whole matrix
/// (eleven levels × the five corpus inputs), so this costs a single startup.
#[test]
fn every_level_and_input_matches_the_live_oracle() {
    let corpus = zsupport::corpus_small();
    let mut cases = Vec::new();
    for level in LEVELS {
        for (_, data) in &corpus {
            cases.push(zsupport::Case::zlib(level, data));
        }
    }
    let Some(answers) = zsupport::oracle_batch(&cases) else {
        println!("SKIP every_level_and_input_matches_the_live_oracle: erl not found");
        return;
    };
    let mut checked = 0usize;
    for (case, answer) in cases.iter().zip(answers) {
        assert_eq!(case.window_bits, 15, "the `zlib:compress/1` framing");
        let want = answer
            .unwrap_or_else(|why| panic!("the oracle refused level {}: {why}", case.level));
        let ours = compress(&case.data, case.level).expect("an in-range level");
        assert_eq!(
            hex(&ours),
            hex(&want),
            "level {} of {} bytes",
            case.level,
            case.data.len()
        );
        checked += 1;
    }
    assert_eq!(checked, LEVELS.len() * corpus.len());
    println!("{checked} compress images byte-identical to the oracle");
}

/// A level outside `-1`/`0..=9` is a rejected configuration — `deflateInit2_`'s
/// `Z_STREAM_ERROR`, surfaced as `BadArg` — and never a substituted stream.
#[test]
fn an_out_of_range_level_is_badarg() {
    for level in [10i8, 11, 100, -2, -100, i8::MAX, i8::MIN] {
        assert!(
            matches!(compress(b"abc", level), Err(BeamError::BadArg)),
            "level {level} must be refused"
        );
    }
    // The accepted boundary values stay accepted, so the test above is not vacuous.
    for level in [-1i8, 0, 1, 9] {
        assert!(compress(b"abc", level).is_ok(), "level {level}");
    }
}

// ---------------------------------------------------------------------------
// `decompress_bounded_used` — the ETF `used` contract
// ---------------------------------------------------------------------------

/// With no trailing bytes the count is the stream length; with trailing bytes it is still
/// the stream length, so the junk is neither read nor counted.
#[test]
fn the_consumed_count_excludes_trailing_bytes() {
    let image = a300_image();
    let (body, used) = decompress_bounded_used(&image, usize::MAX).expect("a valid stream");
    assert_eq!(body, vec![b'a'; A300_LEN]);
    assert_eq!(used, image.len(), "no trailing bytes: the whole input");

    // Junk that is not a stream at all: it cannot have been read, or the call would have
    // failed with a data error.
    let mut junk = image.clone();
    junk.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0xff]);
    let (body, used) = decompress_bounded_used(&junk, usize::MAX).expect("junk is not read");
    assert_eq!(body, vec![b'a'; A300_LEN]);
    assert_eq!(used, image.len(), "the junk is not counted");
    assert!(used < junk.len());

    // A second complete stream after the first is trailing data too: `inflate` stops at
    // the first `Z_STREAM_END`, exactly like `binary_to_term/2`'s scan.
    let mut two = image.clone();
    two.extend_from_slice(&image);
    let (_, used) = decompress_bounded_used(&two, usize::MAX).expect("the first stream");
    assert_eq!(used, image.len(), "only the first stream is consumed");
}

/// The bound is an inclusive ceiling: `limit == payload` is accepted, `limit == payload - 1`
/// is refused, and a larger limit changes nothing.
#[test]
fn the_output_bound_is_inclusive() {
    let image = a300_image();
    assert_eq!(
        decompress_bounded(&image, A300_LEN)
            .expect("exactly the payload length")
            .len(),
        A300_LEN
    );
    assert!(
        matches!(decompress_bounded(&image, A300_LEN - 1), Err(BeamError::BadArg)),
        "one byte short of the payload must be refused"
    );
    assert!(
        matches!(decompress_bounded(&image, 0), Err(BeamError::BadArg)),
        "a zero bound cannot accept a non-empty payload"
    );
    for limit in [A300_LEN + 1, 1 << 16, usize::MAX] {
        assert_eq!(
            decompress_bounded(&image, limit)
                .unwrap_or_else(|e| panic!("limit {limit}: {e:?}"))
                .len(),
            A300_LEN,
            "limit {limit}"
        );
    }
    // An empty payload fits in a zero bound, so the refusal above is about the payload and
    // not about the limit being zero.
    let empty = compress(b"", DEFAULT_COMPRESSION).expect("compress empty");
    assert!(decompress_bounded(&empty, 0).expect("zero bytes").is_empty());
}

/// The three decode entry points agree, and `decompress_bounded`'s body is exactly the
/// `used` form's.
#[test]
fn the_three_decode_forms_agree() {
    for ((name, data), level) in zsupport::corpus_small()
        .iter()
        .flat_map(|entry| LEVELS.iter().map(move |level| (entry, level)))
    {
        let image = compress(data, *level).expect("an in-range level");
        let (bounded_body, used) =
            decompress_bounded_used(&image, usize::MAX).expect("a stream this port built");
        assert_eq!(bounded_body, *data, "{name} at level {level}: bounded_used");
        assert_eq!(used, image.len(), "{name} at level {level}: consumed");
        assert_eq!(
            decompress_bounded(&image, usize::MAX).expect("bounded"),
            *data,
            "{name} at level {level}: bounded"
        );
        assert_eq!(
            decompress(&image).expect("unbounded"),
            *data,
            "{name} at level {level}: unbounded"
        );
    }
}

// ---------------------------------------------------------------------------
// The unbounded path and a multi-MiB payload
// ---------------------------------------------------------------------------

/// A 4 MiB payload through `decompress` (`limit = usize::MAX`) neither overflows nor
/// panics, and agrees with the bounded forms at both the exact limit and the limit one
/// byte short.
///
/// This is the `usize::MAX` arithmetic made explicit: the one-shot loop offers
/// `limit.saturating_sub(len).saturating_add(1)` output bytes per pump step, which saturates
/// at `usize::MAX` instead of wrapping, and then clamps to the 16 KiB chunk — so the
/// unbounded route is bounded per call and only the accumulated body grows.
#[test]
fn the_unbounded_form_handles_a_multi_mib_payload() {
    const MIB4: usize = 4 << 20;
    let data: Vec<u8> = (0..MIB4).map(|i| (i % 251) as u8).collect();

    let image = compress(&data, 1).expect("a 4 MiB payload compresses");
    assert!(image.len() < data.len(), "a periodic payload compresses");

    assert_eq!(decompress(&image).expect("unbounded").len(), data.len());
    assert_eq!(decompress(&image).expect("unbounded"), data);

    let (body, used) = decompress_bounded_used(&image, usize::MAX).expect("saturated limit");
    assert_eq!(body, data);
    assert_eq!(used, image.len(), "the whole stream and nothing else");

    // The bound still works at MiB scale: exact is accepted, one short is refused, and it
    // is refused before the body can grow to the payload's size.
    assert_eq!(
        decompress_bounded(&image, data.len())
            .expect("the exact limit")
            .len(),
        data.len()
    );
    assert!(matches!(
        decompress_bounded(&image, data.len() - 1),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

/// Every malformed shape is `BadArg`, never a panic and never a silently short body.
#[test]
fn malformed_streams_are_badarg() {
    let image = a300_image();

    // Nothing at all.
    assert!(matches!(decompress_bounded_used(&[], usize::MAX), Err(BeamError::BadArg)));
    assert!(matches!(decompress_bounded(&[], 1024), Err(BeamError::BadArg)));
    assert!(matches!(decompress(&[]), Err(BeamError::BadArg)));

    // A single header byte (`0x78` of the two-byte zlib header).
    assert!(matches!(
        decompress_bounded_used(&image[..1], usize::MAX),
        Err(BeamError::BadArg)
    ));
    // A valid header with no body: the CMF/FLG pair alone.
    assert!(matches!(
        decompress_bounded_used(&image[..2], usize::MAX),
        Err(BeamError::BadArg)
    ));

    // Every truncation prefix. A stream that stops anywhere before its trailer has no
    // `Z_STREAM_END`, so the one-shot loop must refuse it.
    for cut in 0..image.len() {
        assert!(
            matches!(
                decompress_bounded_used(&image[..cut], usize::MAX),
                Err(BeamError::BadArg)
            ),
            "truncated to {cut} of {} bytes",
            image.len()
        );
        // The same prefix under a tight bound is refused as well (whichever check fires
        // first, it is `BadArg`).
        assert!(
            matches!(decompress_bounded_used(&image[..cut], 8), Err(BeamError::BadArg)),
            "truncated to {cut} bytes under a bound of 8"
        );
    }

    // A corrupt Adler-32 trailer: each of the four bytes is part of the check.
    for bad_byte in 1..=4 {
        let mut corrupt = image.clone();
        let at = corrupt.len() - bad_byte;
        corrupt[at] ^= 0xff;
        assert!(
            matches!(
                decompress_bounded_used(&corrupt, usize::MAX),
                Err(BeamError::BadArg)
            ),
            "the trailer byte at {at} must be checked"
        );
    }

    // A corrupt header: the CMF/FLG pair carries the method/window and the mod-31 check.
    for at in 0..2 {
        let mut corrupt = image.clone();
        corrupt[at] ^= 0x01;
        assert!(
            matches!(
                decompress_bounded_used(&corrupt, usize::MAX),
                Err(BeamError::BadArg)
            ),
            "the header byte at {at} must be checked"
        );
    }

    // Bits flipped inside the compressed body: either the block structure breaks
    // (`DataError`) or the payload's Adler-32 no longer matches. A few bit positions are
    // never read at all — the padding after the final end-of-block code and code space a
    // 300-byte payload does not use — and those decode to *exactly* the same bytes. What
    // must never happen is a silently different body.
    let mut refused = 0usize;
    let mut unread = Vec::new();
    let mut total = 0usize;
    for at in 2..image.len() {
        for bit in [0x01u8, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80] {
            total += 1;
            let mut corrupt = image.clone();
            corrupt[at] ^= bit;
            match decompress_bounded_used(&corrupt, usize::MAX) {
                Err(BeamError::BadArg) => refused += 1,
                Ok((body, used)) => {
                    assert_eq!(
                        body,
                        vec![b'a'; A300_LEN],
                        "the flipped bit at byte {at}, mask {bit:#04x} silently changed the body"
                    );
                    assert_eq!(used, image.len(), "a benign flip still consumes its stream");
                    unread.push((at, bit));
                }
                Err(other) => panic!("byte {at}, mask {bit:#04x}: unexpected {other:?}"),
            }
        }
    }
    assert_eq!(refused + unread.len(), total);
    assert_eq!(
        unread, UNREAD_BIT_POSITIONS,
        "the unread bit positions are a property of the data, and the oracle's zlib reports \
         exactly these six for this image"
    );
    assert_eq!(refused, total - UNREAD_BIT_POSITIONS.len());
    println!("{refused} of {total} single-bit body flips refused, {total} - {refused} unread");

    // Not a zlib stream at all.
    assert!(matches!(decompress(b"not zlib"), Err(BeamError::BadArg)));
    assert!(matches!(decompress(&[0xff; 64]), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The ETF `COMPRESSED` shape, end to end
// ---------------------------------------------------------------------------

/// The image `term_to_binary(42, [compressed])` is: `[131][80][InnerSize:4 BE][stream]`.
///
/// This builds that shape around [`compress`]/[`decompress_bounded_used`] and checks it
/// against the real consumer — the oracle's `binary_to_term/2` — so the `used` offset the
/// `beam_codec` decoder has to add (`6 + stream`) is pinned by the C side, not by this
/// crate's own convention.
#[test]
fn the_etf_compressed_shape_round_trips_and_the_oracle_agrees() {
    /// `term_to_binary(42)` — `131` (version), `97` (`SMALL_INTEGER_EXT`), `42`.
    const INNER_TERM: [u8; 3] = [131, 97, 42];
    const HEADER_LEN: usize = 6;

    let stream = compress(&INNER_TERM, DEFAULT_COMPRESSION).expect("compress");
    let mut image = vec![131u8, 80];
    image.extend_from_slice(&(INNER_TERM.len() as u32).to_be_bytes());
    image.extend_from_slice(&stream);
    // A second term after the compressed one: `binary_to_term/2` must not count it.
    image.extend_from_slice(&[131, 97, 7]);

    assert_eq!(&image[HEADER_LEN..HEADER_LEN + stream.len()], &stream[..]);

    let (body, used) =
        decompress_bounded_used(&image[HEADER_LEN..], 1 << 20).expect("the payload");
    assert_eq!(body, INNER_TERM, "the inner term's ETF bytes");
    assert_eq!(used, stream.len(), "the stream and nothing after it");

    if let Some(out) = zsupport::erl_eval(&format!(
        "B = binary:decode_hex(<<\"{}\">>), \
         {{Term, Used}} = binary_to_term(B, [used]), \
         io:format(\"~p ~p~n\", [Term, Used]), halt().",
        hex(&image)
    )) {
        let reply = out.trim();
        let mut parts = reply.split_whitespace();
        let term = parts.next().unwrap_or_default();
        let used_oracle: usize = parts
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("unparsable oracle reply {reply:?}"));
        assert_eq!(term, "42", "the oracle decoded the same term");
        assert_eq!(
            used_oracle,
            HEADER_LEN + used,
            "`binary_to_term/2`'s `used` is the 6-byte header plus the stream"
        );
    } else {
        println!("SKIP the_etf_compressed_shape_round_trips_and_the_oracle_agrees: erl not found");
    }
}

// ---------------------------------------------------------------------------
// The round trip
// ---------------------------------------------------------------------------

/// `decompress(compress(x, level)) == x` for a few hundred deterministic inputs — ten sizes
/// (0, 1, 2, 3, 7, 255, 256, 512, 4096, 65536) × four generators × every level — and the
/// same claim under the exact bound.
#[test]
fn the_corpus_round_trips_at_every_level() {
    let mut checked = 0usize;
    for (shape, generate) in generators() {
        for size in SIZES {
            let data = generate(size);
            for level in LEVELS {
                let image = compress(&data, level).unwrap_or_else(|e| {
                    panic!("{shape} of {size} bytes at level {level}: {e:?}")
                });
                let back = decompress(&image).unwrap_or_else(|e| {
                    panic!("{shape} of {size} bytes at level {level}: {e:?}")
                });
                assert_eq!(back.len(), data.len(), "{shape}/{size}/level {level}: length");
                assert_eq!(back, data, "{shape}/{size}/level {level}: bytes");
                assert_eq!(
                    decompress_bounded(&image, size)
                        .unwrap_or_else(|e| panic!(
                            "{shape} of {size} bytes at level {level}: {e:?}"
                        ))
                        .len(),
                    size,
                    "{shape}/{size}/level {level}: the exact bound"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, generators().len() * SIZES.len() * LEVELS.len());
    println!("{checked} round trips");
}

// ---------------------------------------------------------------------------
// Golden regeneration (needs `erl`; never runs in a plain `cargo test`)
// ---------------------------------------------------------------------------

/// Regenerate the two embedded oracle tables, asserting first that this port already
/// produces the oracle's bytes — the route documented at the top of this file.
#[test]
#[ignore = "regenerates the embedded oracle images; needs erl, paste the output back"]
fn regenerate_lib_api_goldens() {
    let corpus = zsupport::corpus_small();
    let mut cases: Vec<zsupport::Case> = corpus
        .iter()
        .map(|(_, data)| zsupport::Case::zlib(DEFAULT_COMPRESSION, data))
        .collect();
    for level in 0..=9i8 {
        cases.push(zsupport::Case::zlib(level, b"abc"));
    }
    let Some(answers) = zsupport::oracle_batch(&cases) else {
        println!("SKIP regenerate_lib_api_goldens: erl not found");
        return;
    };
    let mut images = Vec::new();
    for (case, answer) in cases.iter().zip(answers) {
        let want = answer.expect("the oracle accepted every case");
        let ours = compress(&case.data, case.level).expect("an in-range level");
        assert_eq!(
            hex(&ours),
            hex(&want),
            "level {} of {} bytes diverges",
            case.level,
            case.data.len()
        );
        images.push(hex(&want));
    }
    let split = corpus.len();
    print_string_table("DEFAULT_COMPRESSION_IMAGES", &images[..split]);
    print_string_table("ABC_LEVEL_IMAGES", &images[split..]);
}

/// Print one `const NAME: [&str; N] = [...]` between `=== NAME ===` markers, wrapping the
/// hex at 92 characters so the table stays readable.
fn print_string_table(name: &str, images: &[String]) {
    println!("=== {name} ===");
    println!("const {name}: [&str; {}] = [", images.len());
    for (index, image_hex) in images.iter().enumerate() {
        println!("    // case {index}");
        if image_hex.is_empty() {
            println!("    \"\",");
            continue;
        }
        println!("    concat!(");
        for chunk in image_hex.as_bytes().chunks(92) {
            let chunk = std::str::from_utf8(chunk).expect("hex is ASCII");
            println!("        \"{chunk}\",");
        }
        println!("    ),");
    }
    println!("];");
}
