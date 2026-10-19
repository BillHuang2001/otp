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

//! The **byte-exact golden** suite for the `zlib` stream API: the 19 keys of
//! [`super::NIF_KEYS`], driven through the crate's own [`BifRegistry`] exactly the way
//! `zlib.erl`'s wrappers drive them (`deflate/3` → `restore_progress` → `enqueue_input`
//! → `dequeue_all_chunks`, and so on).
//!
//! # Why this file is long
//!
//! It is a *golden* suite, so every value the oracle produced is embedded here as a size
//! vector, a hex image or an md5 — no test shells out to `erl`, which keeps the suite
//! self-contained and CI-safe without an Erlang installation. The goldens are the ones
//! `../../ZLIB_STREAM_BRIEF.md` § *Values that must hold (oracle OTP 29.1.1,
//! live-verified)* records; each deflate-side image was re-derived independently with
//! zlib 1.3.2 itself (`zlib.compressobj` over the same fixture, same parameters) before
//! being pasted in, so a mismatch is a real divergence on one side or the other rather
//! than a transcription slip. The convention — an embedded golden table plus the whole
//! matrix around it — is the same one `beam_deflate/tests/deflate_goldens.rs` follows.
//!
//! The file is `#[cfg(test)]`-gated (declared by [`super`] as `#[cfg(test)] mod tests;`),
//! so it cannot affect a release build.
//!
//! # Ground rules
//!
//! * **Byte identity or a typed error, never "close enough."** A failing golden is a
//!   defect to report, not an assertion to relax.
//! * The stream registry is a process-global `OnceLock`, so every test mints its own
//!   handles (ids are distinct and monotonic) and none assumes it runs alone.
//! * `unsafe` cannot appear here (`#![deny(unsafe_code)]` at the crate root), so the two
//!   terms a test cannot build with safe accessors — a remote pid — is decoded from a
//!   hand-built `NEW_PID_EXT` image instead.

use beam_alloc::Heap;
use beam_common::Result;
use beam_deflate::{Deflate, DeflateConfig, Flush};
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::registry::BifRegistry;

use super::{assert_badarg, assert_raise_atom, lock_streams, State};

// ---------------------------------------------------------------------------
// The oracle fixtures
// ---------------------------------------------------------------------------

/// The sentence every fixture is built from — the one
/// `../../ZLIB_STREAM_BRIEF.md` names for `F1`.
const SENTENCE: &[u8] = b"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n";

/// `F1` — the 1024-byte pump fixture: `binary:part(binary:copy(Sentence, 6), 0, 1024)`.
fn f1() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1024);
    while bytes.len() < 1024 {
        bytes.extend_from_slice(SENTENCE);
    }
    bytes.truncate(1024);
    bytes
}

/// The 100 000-byte payload behind the `safeInflate/2` chunk-sequence golden
/// (`SENTENCE` repeated, truncated): it compresses to 494 B at level 6, which is what
/// makes the 1:1 `{continue, [16384]} ×6` / `{finished, [1696]}` sequence observable.
fn payload_100k() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(100_000);
    while bytes.len() < 100_000 {
        bytes.extend_from_slice(SENTENCE);
    }
    bytes.truncate(100_000);
    bytes
}

/// The per-level sync-flush vectors: `(level, leaf sizes of the five calls, total,
/// md5 of the concatenation)`.
///
/// The sequence is `deflateInit(Z, Level)`, then `4 × deflate(Z, 256 B, sync)`, then
/// `deflate(Z, [], finish)` — one leaf per call, so each entry lists exactly five sizes.
const LEVEL_GOLDENS: &[(i64, [usize; 5], usize, &str)] = &[
    (0, [268, 266, 266, 266, 9], 1075, "744E6AB1F1679A65419D31433B4CE009"),
    (1, [102, 11, 9, 9, 6], 137, "1483D581C7312DE6CF3B5536AC760D8D"),
    (2, [100, 11, 9, 9, 6], 135, "CA8610AA9D9C8629E0A160295B113D3D"),
    (3, [100, 11, 9, 9, 6], 135, "CA8610AA9D9C8629E0A160295B113D3D"),
    (4, [96, 9, 9, 9, 6], 129, "A33B7229B7EDF94FDA7E879668E71BDE"),
    (5, [96, 9, 9, 9, 6], 129, "A33B7229B7EDF94FDA7E879668E71BDE"),
    (6, [96, 9, 9, 9, 6], 129, "559A0ED1917748B7D484AE0BF201F1DD"),
    (7, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
    (8, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
    (9, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
];

/// Level 6's whole 129-byte sync-flush stream (`LEVEL_GOLDENS`' level-6 row, spelled
/// out so the bytes — not only the md5 — are pinned).
const LEVEL6_STREAM: &str = "789CB4CEC10980301044D1BB556C01621F29439389598859C9AE04BB979C44D0A3E7F707C619B559C91268811A4924E30D3A12DFD2A47ED1BCA243630DB2BD4914C9ACA9409F43ECE2530F1664467C372EBE221C99ED9C06F7C7D50B0000FFFF1AE94E05000000FFFF1AE94E05000000FFFF1AE94E05000000FFFF0300499E6670";

/// `deflate(Z, F1, finish)` at level 6 / windowBits 15 / memLevel 8 / default strategy.
const F1_ONE_SHOT_LEVEL6: &str = "789CEDCEC10980301044D1BB556C01621F96A171621692ACB82BC1EEC59308F1E631E737037F342A939205D00C35124FC609DA133F5264FFA269C50D85759154132F12594386BE8FD8C4857B3023327CDD38BB1DCB11D9CEA11B5B6A4B6DA9FFA65E499E6670";

/// The preset-dictionary stream for [`DICT`]/[`DICT_PAYLOAD`] (`deflate(finish)`, 15 B).
const DICT_STREAM: &str = "78BB9EB8166023240F21011D420E92";

/// The dictionary of the `need_dictionary` protocol.
const DICT: &[u8] = b"a dictionary with repeated words words words and more words";

/// The payload compressed *against* [`DICT`].
const DICT_PAYLOAD: &[u8] = b"words words words and more words words";

/// `zlib:deflateSetDictionary/2`'s answer for [`DICT`] — its Adler-32.
const DICT_ADLER: i64 = 2_662_864_480;

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

/// Uppercase hex, the spelling the goldens use.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0').to_ascii_uppercase());
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0').to_ascii_uppercase());
    }
    out
}

/// Decodes an uppercase or lowercase hex string (non-hex digits are skipped, so the
/// goldens may be line-wrapped).
fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        })
        .collect();
    assert_eq!(digits.len() % 2, 0, "hex must have an even digit count");
    digits.chunks(2).map(|pair| (pair[0] << 4) | pair[1]).collect()
}

/// A compact RFC 1321 MD5 as uppercase hex.
///
/// The crate's own digests live behind the `erlang:md5/*` BIFs (`src/checksum.rs`), whose
/// digest function is private, and the goldens are expressed as md5s — so the test module
/// carries the four-line-per-round reference implementation rather than widening the
/// BIF module's surface.
fn md5(bytes: &[u8]) -> String {
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    #[rustfmt::skip]
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in message.chunks(64) {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for round in 0..64 {
            let (f, g) = match round / 16 {
                0 => ((b & c) | (!b & d), round),
                1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                2 => (b ^ c ^ d, (3 * round + 5) % 16),
                _ => (c ^ (b | !d), (7 * round) % 16),
            };
            let swapped = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[round])
                    .wrapping_add(words[g])
                    .rotate_left(S[round]),
            );
            a = swapped;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Context plumbing
// ---------------------------------------------------------------------------

/// A destination heap with room for the 100 KiB payload and its 16 KiB output chunks.
fn heap() -> Heap {
    Heap::new(1 << 21).expect("test heap")
}

/// The atom vocabulary every body needs: the status/tag atoms
/// ([`crate::zlib::stream::pump::zlib_return`], `outcome_term`, `raise_atom`,
/// `enqueue_nif`'s `continue`) plus the names the stash tests store.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "ok",
        "empty",
        "continue",
        "finished",
        "need_dictionary",
        "stream_end",
        "stream_error",
        "data_error",
        "mem_error",
        "buf_error",
        "unknown_error",
        "not_initialized",
        "already_initialized",
        "not_on_controlling_process",
        "error",
        "undefined",
        "true",
        "false",
        "hello",
        "deflate",
        "inflate",
        "abc",
        "k",
        "v",
    ]);
    atoms
}

fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable, pid: ProcessId) -> BifContext<'a> {
    BifContext::new(heap, atoms, pid)
}

/// A registry carrying exactly the 19 stream keys (no other BIF, so a miss is loud).
fn registry() -> BifRegistry {
    let mut reg = BifRegistry::new();
    super::register(&mut reg);
    assert_eq!(reg.len(), 19, "the stream module registers 19 keys");
    reg
}

fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call("zlib", name, args.len() as u32, ctx, args)
}

fn must(reg: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, args: &[Eterm]) -> Eterm {
    call(reg, ctx, name, args).unwrap_or_else(|error| panic!("zlib:{name} failed: {error:?}"))
}

// ---------------------------------------------------------------------------
// Stream plumbing — the wrappers' own sequences, one NIF call at a time
// ---------------------------------------------------------------------------

/// `zlib:open/0` → the handle of a fresh `ST_NONE` stream owned by the caller.
fn open(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Eterm {
    let handle = must(reg, ctx, "open_nif", &[]);
    assert!(
        access::is_reference(handle),
        "open_nif answers a reference"
    );
    handle
}

/// `deflateInit(Z, Level, Method, WindowBits, MemLevel, Strategy)`.
fn deflate_init(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    level: i64,
    method: i64,
    window_bits: i64,
    mem_level: i64,
    strategy: i64,
) -> Result<Eterm> {
    let args = [
        handle,
        ctx.int(level)?,
        ctx.int(method)?,
        ctx.int(window_bits)?,
        ctx.int(mem_level)?,
        ctx.int(strategy)?,
    ];
    call(reg, ctx, "deflateInit_nif", &args)
}

/// `zlib:deflateInit(Z, Level)` — `deflated`/15/8/`default`.
fn deflate_init_default(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    level: i64,
) -> Eterm {
    must(
        reg,
        ctx,
        "deflateInit_nif",
        &[
            handle,
            ctx.int(level).expect("small int"),
            ctx.int(8).expect("small int"),
            ctx.int(15).expect("small int"),
            ctx.int(8).expect("small int"),
            ctx.int(0).expect("small int"),
        ],
    )
}

/// `inflateInit(Z, WindowBits, EosBehaviour)`.
fn inflate_init(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    window_bits: i64,
    eos_behavior: i64,
) -> Result<Eterm> {
    let args = [handle, ctx.int(window_bits)?, ctx.int(eos_behavior)?];
    call(reg, ctx, "inflateInit_nif", &args)
}

/// `enqueue_nif/2` over one iovec piece.
fn enqueue(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    bytes: &[u8],
) -> Result<Eterm> {
    enqueue_pieces(reg, ctx, handle, &[bytes])
}

/// `enqueue_nif/2` over a whole iovec (one segment per slice).
fn enqueue_pieces(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    pieces: &[&[u8]],
) -> Result<Eterm> {
    let mut segments = Vec::with_capacity(pieces.len());
    for piece in pieces {
        segments.push(bits::make_binary(ctx.heap(), piece)?);
    }
    let iovec = ctx.list(&segments)?;
    call(reg, ctx, "enqueue_nif", &[handle, iovec])
}

/// `deflate_nif/4` — one `zlib_codec` call.
fn deflate_call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Eterm> {
    let args = [
        handle,
        ctx.int(in_chunk)?,
        ctx.int(out_chunk)?,
        ctx.int(flush)?,
    ];
    call(reg, ctx, "deflate_nif", &args)
}

/// `inflate_nif/4` — one `zlib_codec` call.
fn inflate_call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Eterm> {
    let args = [
        handle,
        ctx.int(in_chunk)?,
        ctx.int(out_chunk)?,
        ctx.int(flush)?,
    ];
    call(reg, ctx, "inflate_nif", &args)
}

/// The flush codes `zlib.erl`'s `arg_flush/1` produces.
const NONE: i64 = 0;
const SYNC: i64 = 2;
const FINISH: i64 = 4;

/// `zlib.erl`'s chunk sizes (`?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE` and the
/// inflate pair).
const DEFLATE_CHUNK: i64 = 8192;
const INFLATE_IN_CHUNK: i64 = 8192;
const INFLATE_OUT_CHUNK: i64 = 16384;

// ---------------------------------------------------------------------------
// Reading an answer
// ---------------------------------------------------------------------------

/// The atoms an answer can carry, for [`name_of`].
const ANSWER_ATOMS: &[&str] = &[
    "finished",
    "continue",
    "need_dictionary",
    "ok",
    "error",
    "empty",
    "stream_end",
    "mem_error",
    "buf_error",
];

/// The name of `term` when it is one of [`ANSWER_ATOMS`], else `None`.
fn name_of(ctx: &BifContext<'_>, term: Eterm) -> Option<&'static str> {
    for name in ANSWER_ATOMS {
        if ctx.atom(name).map(|atom| atom == term).unwrap_or(false) {
            return Some(name);
        }
    }
    None
}

/// One `deflate_nif`/`inflate_nif` answer, split into the parts a golden is stated in.
struct Answer {
    /// The C's tag atom (`finished` / `continue` / `need_dictionary`) when the
    /// `{Tag, Out}` tuple is there, `None` when the answer is a bare output list. Kept
    /// as the raw term so that reading an answer never borrows the context.
    tag: Option<Eterm>,
    /// Every output leaf, in order.
    leaves: Vec<Vec<u8>>,
    /// `{need_dictionary, Adler, Out}`'s Adler value.
    adler: Option<i64>,
}

impl Answer {
    /// The leaf sizes — the shape every deflate golden is stated as.
    fn sizes(&self) -> Vec<usize> {
        self.leaves.iter().map(Vec::len).collect()
    }

    /// All the produced bytes.
    fn bytes(&self) -> Vec<u8> {
        self.leaves.concat()
    }
}

/// Splits an answer into its tag, its leaves and its Adler value.
///
/// The C answers `{finished, Out}`, `{continue, Out}` or `{need_dictionary, Adler, Out}`
/// (`zlib_nif.c:395-468`) — `zlib.erl`'s `dequeue_all_chunks_1/3` pattern-matches all
/// three. A bare list (or bare atom) is recorded with `tag: None` so the byte goldens can
/// still be read; the tag assertions then report the missing tag on its own.
fn answer(term: Eterm) -> Answer {
    if access::is_tuple(term) && access::tuple_size(term) >= 2 {
        let arity = access::tuple_size(term);
        let adler = if arity == 3 {
            let value = access::tuple_element(term, 1);
            assert!(access::is_small(value), "the dictionary Adler is an integer");
            Some(beam_term::signed_value(value))
        } else {
            None
        };
        return Answer {
            tag: Some(access::tuple_element(term, 0)),
            leaves: list_bytes(access::tuple_element(term, arity - 1)),
            adler,
        };
    }
    if access::is_list(term) || access::is_nil(term) {
        return Answer {
            tag: None,
            leaves: list_bytes(term),
            adler: None,
        };
    }
    Answer {
        tag: Some(term),
        leaves: Vec::new(),
        adler: None,
    }
}

/// The bytes of each element of a proper list of binaries (an output iolist).
fn list_bytes(term: Eterm) -> Vec<Vec<u8>> {
    let mut leaves = Vec::new();
    let mut rest = term;
    while !access::is_nil(rest) {
        assert!(
            access::is_list(rest),
            "an output iolist is a proper list of binaries"
        );
        leaves.push(bits::bytes(access::car(rest)).expect("an output leaf is a binary"));
        rest = access::cdr(rest);
    }
    leaves
}

/// Asserts the C's answer tag.
fn assert_tag(ctx: &BifContext<'_>, got: &Answer, want: &str, what: &str) {
    let atom = ctx.atom(want).expect("a seeded tag atom");
    assert_eq!(
        got.tag,
        Some(atom),
        "{what}: the answer must be the C's `{{{want}, Out}}` tuple, whose tag is the atom \
         `{want}` (zlib.erl's dequeue_all_chunks_1/3 matches on it); a bare output list \
         loses the tag and drives the Erlang layer into a case_clause"
    );
}

// ---------------------------------------------------------------------------
// Fixture construction (independent of the code under test)
// ---------------------------------------------------------------------------

/// One-shot compression through `beam_deflate` — used only to *build* fixtures, so a
/// defect in a `zlib` NIF cannot hide behind a fixture the same NIF produced.
fn deflate_fixture(config: DeflateConfig, body: &[u8]) -> Vec<u8> {
    let mut deflate = Deflate::new(config).expect("a legal configuration");
    let mut out = vec![0u8; body.len() + 1024];
    let step = deflate
        .deflate(body, &mut out, Flush::Finish)
        .expect("the finish step");
    assert_eq!(step.consumed, body.len(), "the whole fixture body");
    out.truncate(step.produced);
    out
}

/// The level-6 zlib framing of `body` (fixture construction only).
fn zlib_image(body: &[u8]) -> Vec<u8> {
    deflate_fixture(DeflateConfig::zlib(6), body)
}

/// A `NEW_PID_EXT` image decoded into a term — the remote pid `set_controller_nif/2`
/// must refuse (`enif_get_local_pid`).
fn remote_pid(ctx: &mut BifContext<'_>) -> Eterm {
    let node = b"remote@host";
    let mut image = vec![131u8, 88, 118, 0, node.len() as u8];
    image.extend_from_slice(node);
    image.extend_from_slice(&1u32.to_be_bytes()); // id
    image.extend_from_slice(&0u32.to_be_bytes()); // serial
    image.extend_from_slice(&0u32.to_be_bytes()); // creation
    let mut scratch = AtomTable::new();
    let term = beam_codec::decode_term(&image, ctx.heap(), &mut scratch).expect("a pid image");
    assert!(access::is_external_pid(term), "the decoded pid is remote");
    term
}

/// The stream registry entry behind `handle` — the white-box view the drop assertions
/// need (a closed stream's registry entry is unreachable through the NIFs by design).
fn stream_state(handle: Eterm) -> (State, bool, usize, bool, bool) {
    let id = access::ref_thing_id(handle).expect("a stream handle");
    let streams = lock_streams();
    let stream = streams.get(&id).expect("a registered stream");
    (
        stream.state,
        stream.deflate.is_some(),
        stream.queue.len(),
        stream.stash.is_some(),
        stream.eos_seen,
    )
}

// ===========================================================================
// A. The pump, against the embedded goldens
// ===========================================================================

/// `F1` is the fixture every deflate golden is stated over, so it is pinned by md5 before
/// anything is compressed: 1024 B of the brief's sentence.
#[test]
fn the_f1_fixture_is_the_one_the_goldens_were_measured_on() {
    let fixture = f1();
    assert_eq!(fixture.len(), 1024);
    assert_eq!(SENTENCE.len(), 171, "the sentence's own length");
    assert_eq!(
        md5(&fixture),
        "A1D3CA40959CD47D539D2A0115E99AF2",
        "F1's md5 (the brief's fixture pin)"
    );
    assert_eq!(
        md5(&payload_100k().len().to_be_bytes()),
        md5(&100_000u32.to_be_bytes()),
        "the safeInflate payload is 100 000 B"
    );
}

/// A.1 — for every level `0..=9`: the leaf-size vector, the total and the md5 of the
/// concatenation of `4 × deflate(256 B, sync)` followed by `deflate([], finish)`.
///
/// This is the pump's whole accounting in one matrix: one leaf per call, the sync marker
/// at each boundary, the final block and the Adler-32 trailer.
#[test]
fn every_level_produces_the_oracles_sync_flush_leaf_vectors() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let fixture = f1();

    for &(level, sizes, total, digest) in LEVEL_GOLDENS {
        let handle = open(&reg, &mut ctx);
        deflate_init_default(&reg, &mut ctx, handle, level);

        let mut per_call: Vec<Vec<usize>> = Vec::new();
        let mut tags: Vec<Option<Eterm>> = Vec::new();
        let mut stream = Vec::new();
        for round in 0..4 {
            let piece = &fixture[round * 256..(round + 1) * 256];
            enqueue(&reg, &mut ctx, handle, piece).expect("enqueue");
            let reply = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, SYNC)
                    .expect("a sync flush"),
            );
            for size in reply.sizes() {
                assert!(
                    size <= DEFLATE_CHUNK as usize,
                    "level {level} round {round}: no leaf may exceed the 8 KiB output chunk"
                );
            }
            stream.extend(reply.bytes());
            per_call.push(reply.sizes());
            tags.push(reply.tag);
        }
        let reply = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
                .expect("a finish"),
        );
        for size in reply.sizes() {
            assert!(
                size <= DEFLATE_CHUNK as usize,
                "level {level}: the finish leaf fits the output chunk"
            );
        }
        stream.extend(reply.bytes());
        per_call.push(reply.sizes());
        tags.push(reply.tag);

        assert_eq!(per_call.len(), 5, "level {level}: five calls");
        for (call_index, &want) in sizes.iter().enumerate() {
            assert_eq!(
                per_call[call_index],
                vec![want],
                "level {level} call {call_index}: the oracle's leaf sizes"
            );
        }
        assert_eq!(stream.len(), total, "level {level}: the total byte count");
        assert_eq!(
            md5(&stream),
            digest,
            "level {level}: the md5 of the concatenated stream ({} B)",
            stream.len()
        );
        // The last thing checked, so a byte defect above is reported first.
        for (call_index, tag) in tags.iter().enumerate() {
            assert_eq!(
                *tag,
                Some(ctx.atom("finished").expect("seeded")),
                "level {level} call {call_index}: a sync-flush call whose queue drained and \
                 whose buffer was not filled must answer `{{finished, Out}}`"
            );
        }
    }
}

/// A.2 — level 6's whole 129-byte stream, byte for byte (the md5 alone is in A.1).
#[test]
fn the_level_six_sync_flush_stream_is_byte_exact() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let fixture = f1();

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    let mut stream = Vec::new();
    for round in 0..4 {
        let piece = &fixture[round * 256..(round + 1) * 256];
        enqueue(&reg, &mut ctx, handle, piece).expect("enqueue");
        let out = deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, SYNC)
            .expect("a sync flush");
        stream.extend(answer(out).bytes());
    }
    let out = deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
        .expect("a finish");
    stream.extend(answer(out).bytes());

    let want = unhex(LEVEL6_STREAM);
    assert_eq!(want.len(), 129, "the golden's own length");
    assert_eq!(hex(&stream), LEVEL6_STREAM, "level 6's 129-byte stream");
    assert_eq!(stream, want);
}

/// A.3 — the one-shot `deflate(Z, F1, finish)` image and its four framings/strategies.
///
/// `wbits -15` must reproduce exactly the framed image's middle 96 B (the RFC 1950 header
/// and Adler trailer are all the framing adds), `wbits 31` that same body inside a gzip
/// member, and the two degenerate strategies the same 546-byte expansion.
#[test]
fn the_one_shot_images_match_the_oracles_framings_and_strategies() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let fixture = f1();

    // Level 6 / windowBits 15 / memLevel 8 / default strategy.
    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("a finish"),
    );
    assert_eq!(out.sizes(), vec![102], "the one-shot leaf size");
    assert_eq!(hex(&out.bytes()), F1_ONE_SHOT_LEVEL6, "the 102-byte image");
    assert_eq!(
        md5(&out.bytes()),
        "6518CCA11F87CB76E961FB89CC729D7A",
        "the one-shot md5"
    );
    assert_tag(&ctx, &out, "finished", "the one-shot finish");

    let framed = unhex(F1_ONE_SHOT_LEVEL6);
    let body = &framed[2..framed.len() - 4];

    // windowBits -15: raw RFC 1951 — the framed image minus its header and Adler-32.
    let handle = open(&reg, &mut ctx);
    deflate_init(&reg, &mut ctx, handle, 6, 8, -15, 8, 0).expect("deflateInit");
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("a finish"),
    );
    assert_eq!(out.sizes(), vec![96], "the raw leaf size");
    assert_eq!(out.bytes(), body.to_vec(), "raw == the framed body");
    assert_eq!(
        md5(&out.bytes()),
        "D4C219FCE1CCF7630F2F3C8468048FD2",
        "the raw md5"
    );

    // windowBits 31: the gzip member — the fixed 10-byte header, the same body, and the
    // little-endian CRC-32 + ISIZE trailer.
    let handle = open(&reg, &mut ctx);
    deflate_init(&reg, &mut ctx, handle, 6, 8, 31, 8, 0).expect("deflateInit");
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("a finish"),
    );
    let mut want_gzip = unhex("1F8B0800000000000003");
    want_gzip.extend_from_slice(body);
    want_gzip.extend_from_slice(&unhex("4FDC6D0200040000"));
    assert_eq!(out.sizes(), vec![114], "the gzip leaf size");
    assert_eq!(hex(&out.bytes()), hex(&want_gzip), "the gzip member");

    // Strategy `filtered` (1): 104 B.
    let handle = open(&reg, &mut ctx);
    deflate_init(&reg, &mut ctx, handle, 6, 8, 15, 8, 1).expect("deflateInit");
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("a finish"),
    );
    assert_eq!(out.sizes(), vec![104], "the filtered leaf size");
    assert_eq!(
        md5(&out.bytes()),
        "A3DBFF853CCF2E0F9A23AB31DF3055AC",
        "the filtered md5"
    );

    // Strategies `huffman_only` (2) and `rle` (3): the same 546-byte expansion.
    for strategy in [2i64, 3] {
        let handle = open(&reg, &mut ctx);
        deflate_init(&reg, &mut ctx, handle, 6, 8, 15, 8, strategy).expect("deflateInit");
        enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
        let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
                .expect("a finish"),
        );
        assert_eq!(out.sizes(), vec![546], "strategy {strategy}: the leaf size");
        assert_eq!(
            md5(&out.bytes()),
            "2FD995AF6E54E35AABA5B103CCBB4059",
            "strategy {strategy}: the md5"
        );
    }
}

/// A.4 — `safeInflate/2`'s 1:1 chunk sequence over the 494-byte image.
///
/// `zlib.erl` hardcodes `?INFLATE_OUT_CHUNKSIZE` at 16 KiB, so the oracle's
/// `{continue, [16384]} ×6` / `{finished, [1696]}` sequence *is* the C's `avail_out`
/// accounting: six full buffers, then the 1696-byte remainder, then `{finished, []}` for
/// the empty follow-up call.
#[test]
fn safe_inflate_delivers_the_oracles_one_to_one_chunk_sequence() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let payload = payload_100k();
    assert_eq!(payload.len(), 100_000);
    assert_eq!(
        md5(&payload),
        "1C99542BAEA58CCE35D22B26DF42F73B",
        "the payload behind the sequence"
    );
    let image = zlib_image(&payload);
    assert_eq!(image.len(), 494, "the brief's 494-byte compressed image");
    assert_eq!(
        md5(&image),
        "2421EBCCB6E4D3D98AEA9DC3C3963707",
        "the compressed image's md5"
    );

    let handle = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit (cut)");
    enqueue(&reg, &mut ctx, handle, &image).expect("enqueue");

    let mut kinds: Vec<Option<Eterm>> = Vec::new();
    let mut sizes: Vec<Vec<usize>> = Vec::new();
    let mut plain = Vec::new();
    for _ in 0..6 {
        let out = answer(inflate_call(
                &reg,
                &mut ctx,
                handle,
                INFLATE_IN_CHUNK,
                INFLATE_OUT_CHUNK,
                NONE,
            )
            .expect("an inflate call"),
        );
        kinds.push(out.tag);
        sizes.push(out.sizes());
        plain.extend(out.bytes());
    }
    let out = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("the last inflate call"),
    );
    kinds.push(out.tag);
    sizes.push(out.sizes());
    plain.extend(out.bytes());

    // The empty follow-up call is `{finished, []}` on the oracle.
    let tail = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("the empty follow-up call"),
    );
    kinds.push(tail.tag);
    sizes.push(tail.sizes());

    let mut want_sizes: Vec<Vec<usize>> = vec![vec![16384], vec![16384], vec![16384]];
    want_sizes.extend([vec![16384], vec![16384], vec![16384]]);
    want_sizes.push(vec![1696]);
    want_sizes.push(vec![]);
    assert_eq!(sizes, want_sizes, "the 1:1 chunk sequence");
    assert_eq!(plain.len(), 100_000, "the whole plaintext");
    assert_eq!(plain, payload, "the plaintext bytes");
    assert_eq!(md5(&plain), md5(&payload), "the plaintext md5");
    // Tag last: a byte defect above is reported first.
    let mut want_kinds: Vec<Option<Eterm>> =
        vec![Some(ctx.atom("continue").expect("seeded")); 6];
    want_kinds.push(Some(ctx.atom("finished").expect("seeded")));
    want_kinds.push(Some(ctx.atom("finished").expect("seeded")));
    assert_eq!(kinds, want_kinds, "the continue/finished sequence");
}

/// A.5 — the preset-dictionary protocol, both directions.
#[test]
fn the_preset_dictionary_protocol_matches_the_oracle() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    // Deflate: `deflateInit(Z, default)` → `deflateSetDictionary` → `deflate(payload,
    // finish)`.
    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, -1);
    let dictionary = bits::make_binary(ctx.heap(), DICT).expect("a binary");
    let adler = must(&reg, &mut ctx, "deflateSetDictionary_nif", &[handle, dictionary]);
    assert!(access::is_small(adler), "the Adler-32 is an integer");
    assert_eq!(
        beam_term::signed_value(adler),
        DICT_ADLER,
        "deflateSetDictionary answers the dictionary's Adler-32"
    );
    enqueue(&reg, &mut ctx, handle, DICT_PAYLOAD).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("a finish"),
    );
    assert_eq!(out.sizes(), vec![15], "the dictionary stream's leaf size");
    assert_eq!(hex(&out.bytes()), DICT_STREAM, "the 15-byte dictionary stream");

    // Inflate: the stream asks for the dictionary, then decodes once it is installed.
    let image = unhex(DICT_STREAM);
    let handle = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit (cut)");
    enqueue(&reg, &mut ctx, handle, &image).expect("enqueue");
    let need = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("the need-dictionary call"),
    );
    assert_eq!(need.adler, Some(DICT_ADLER), "the requested Adler-32");
    assert!(need.leaves.is_empty(), "nothing decodes before the dictionary");
    assert_tag(&ctx, &need, "need_dictionary", "the first inflate call");

    let dictionary = bits::make_binary(ctx.heap(), DICT).expect("a binary");
    let ok = must(&reg, &mut ctx, "inflateSetDictionary_nif", &[handle, dictionary]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "inflateSetDictionary → ok");

    let decoded = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("the post-dictionary call"),
    );
    assert_eq!(decoded.bytes(), DICT_PAYLOAD.to_vec(), "the decoded payload");
}

/// A.6 — `deflateParams/3` mid-stream, replayed exactly as the wrapper does it.
///
/// `zlib.erl:352-362` runs `deflate(Z, <<>>, sync)` first and **stashes** its output for
/// the next `deflate/3` (`save_progress/3` → `setStash_nif` → `restore_progress/2`), which
/// is why the finish call reports two leaves — the stashed 26 B and the 8 B the finish
/// itself produces — and why the whole stream is 36 B rather than the 2 + 34 of a naive
/// count.
#[test]
fn deflate_params_mid_stream_reproduces_the_wrappers_sequence() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 0);

    // 1. `deflate(Z, <<"A"*16>>, none)` → the 2-byte zlib header, nothing more.
    enqueue(&reg, &mut ctx, handle, b"AAAAAAAAAAAAAAAA").expect("enqueue");
    let first = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE)
            .expect("the none flush"),
    );
    assert_eq!(first.sizes(), vec![2], "the level-0 header only");
    assert_eq!(hex(&first.bytes()), "7801", "the zlib header");

    // 2. `deflateParams(Z, 9, default)` starts with `deflate(Z, <<>>, sync)`.
    let sync = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, SYNC)
            .expect("the wrapper's sync flush"),
    );
    assert_eq!(sync.sizes(), vec![26], "the pending stored block and its marker");

    // `save_progress(Z, deflate, Progress)` — the `{deflate, Output}` stash shape.
    let progress = {
        let segment = bits::make_binary(ctx.heap(), &sync.bytes()).expect("a binary");
        let list = ctx.list(&[segment]).expect("a list");
        let tag = ctx.atom("deflate").expect("seeded");
        ctx.tuple(&[tag, list]).expect("the stash tuple")
    };
    must(&reg, &mut ctx, "setStash_nif", &[handle, progress]);

    // 3. `deflateParams_nif(Z, 9, default)` → ok.
    let level = ctx.int(9).expect("small int");
    let strategy = ctx.int(0).expect("small int");
    let params = must(&reg, &mut ctx, "deflateParams_nif", &[handle, level, strategy]);
    assert_eq!(name_of(&ctx, params), Some("ok"), "deflateParams → ok");

    // 4. `deflate(Z, <<"BBBB">>, finish)` → `restore_progress` prepends the stashed chunk.
    let restored = must(&reg, &mut ctx, "getStash_nif", &[handle]);
    let restored = answer(restored);
    assert_eq!(restored.leaves.len(), 1, "the stash carries one segment");
    let stash = restored.bytes();
    must(&reg, &mut ctx, "clearStash_nif", &[handle]);

    enqueue(&reg, &mut ctx, handle, b"BBBB").expect("enqueue");
    let last = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("the finish"),
    );
    let mut leaves = vec![stash];
    leaves.extend(last.leaves.clone());
    let sizes: Vec<usize> = leaves.iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![26, 8], "the finish call's two leaves");

    let total = first.bytes().len() + sync.bytes().len() + last.bytes().len();
    assert_eq!(total, 36, "every byte the sequence produced");

    // And the concatenation is a stream that decodes back to the payload.
    let mut image = first.bytes();
    image.extend(sync.bytes());
    image.extend(last.bytes());
    let plain = inflate_whole(&reg, &mut ctx, &image);
    assert_eq!(plain, b"AAAAAAAAAAAAAAAABBBB".to_vec(), "the round trip");
}

/// Decodes `image` through a fresh inflate stream in one `inflate_nif` call — the images
/// here all fit `?INFLATE_IN_CHUNKSIZE`.
fn inflate_whole(reg: &BifRegistry, ctx: &mut BifContext<'_>, image: &[u8]) -> Vec<u8> {
    let handle = open(reg, ctx);
    inflate_init(reg, ctx, handle, 15, 2).expect("inflateInit");
    enqueue(reg, ctx, handle, image).expect("enqueue");
    let out = answer(inflate_call(reg, ctx, handle, INFLATE_IN_CHUNK, INFLATE_OUT_CHUNK, NONE)
            .expect("an inflate call"),
    );
    out.bytes()
}

/// A.7 — a second `deflate(finish)` is `error:stream_error` (zlib's `FINISH_STATE`).
#[test]
fn a_second_finish_raises_stream_error() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let fixture = f1();

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("the first finish"),
    );
    assert_eq!(out.sizes(), vec![102], "the first finish produced the image");

    assert_raise_atom(
        deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH),
        "stream_error",
    );
}

/// A.8 — `deflateReset` after `finish` makes the stream reusable, and the second run is
/// byte-identical to the first (and to a fresh stream's).
#[test]
fn deflate_reset_after_finish_is_reusable() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let fixture = f1();

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let first = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("the first finish"),
    )
    .bytes();
    assert_eq!(hex(&first), F1_ONE_SHOT_LEVEL6, "the first image");

    let ok = must(&reg, &mut ctx, "deflateReset_nif", &[handle]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "deflateReset → ok");

    enqueue(&reg, &mut ctx, handle, &fixture).expect("enqueue");
    let second = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("the second finish"),
    )
    .bytes();
    assert_eq!(second, first, "a reset stream reproduces the image");

    // The same bytes from a brand-new stream, so the reset really is a full rewind.
    let fresh = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, fresh, 6);
    enqueue(&reg, &mut ctx, fresh, &fixture).expect("enqueue");
    let third = answer(deflate_call(&reg, &mut ctx, fresh, DEFLATE_CHUNK, DEFLATE_CHUNK, FINISH)
            .expect("the fresh finish"),
    )
    .bytes();
    assert_eq!(third, first, "a fresh stream produces the same image");
}

// ===========================================================================
// B. The prologue precedence and the lifecycle guards
// ===========================================================================

/// B.9 — `deflate_nif`/`inflate_nif`/`enqueue_nif` on a stream in `ST_NONE` are
/// `not_initialized`.
#[test]
fn an_uninitialised_stream_answers_not_initialized() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    assert_raise_atom(
        deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
        "not_initialized",
    );
    assert_raise_atom(
        inflate_call(&reg, &mut ctx, handle, INFLATE_IN_CHUNK, INFLATE_OUT_CHUNK, NONE),
        "not_initialized",
    );
    assert_raise_atom(enqueue(&reg, &mut ctx, handle, b"abc"), "not_initialized");
    assert_raise_atom(call(&reg, &mut ctx, "deflateReset_nif", &[handle]),
        "not_initialized",
    );
    assert_raise_atom(call(&reg, &mut ctx, "inflateReset_nif", &[handle]),
        "not_initialized",
    );
}

/// B.10 — a foreign process is refused on every key it can reach, and
/// `set_controller_nif` moves ownership (`zlib.erl`'s `controlling_process/2`).
#[test]
fn a_foreign_process_is_refused_until_it_owns_the_stream() {
    let reg = registry();
    let mut owner_heap = heap();
    let atoms = atoms();
    let owner = ProcessId::new(1, 0);
    let foreign = ProcessId::new(2, 0);
    let mut ctx = context(&mut owner_heap, &atoms, owner);

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);

    // The first NIF `zlib:deflate/3` reaches is `getStash_nif/1` (via
    // `restore_progress/2`), so the check has to be in place there.
    {
        let mut other_heap = heap();
        let mut other = context(&mut other_heap, &atoms, foreign);
        assert_raise_atom(call(&reg, &mut other, "getStash_nif", &[handle]),
            "not_on_controlling_process",
        );
        let term = bits::make_binary(other.heap(), b"abc").expect("a binary");
        assert_raise_atom(call(&reg, &mut other, "setStash_nif", &[handle, term]),
            "not_on_controlling_process",
        );
        assert_raise_atom(
            deflate_call(&reg, &mut other, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
            "not_on_controlling_process",
        );
        assert_raise_atom(call(&reg, &mut other, "close_nif", &[handle]),
            "not_on_controlling_process",
        );
        // The state check comes *after* the controller check: a stream in `ST_NONE`
        // reached by a foreign process is still the ownership fault, not `not_initialized`.
        let untouched = open(&reg, &mut other);
        assert_raise_atom(
            deflate_call(&reg, &mut other, untouched, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
            "not_on_controlling_process",
        );
    }

    // Hand the stream over.
    let new_owner = foreign.to_eterm();
    let ok = must(&reg, &mut ctx, "set_controller_nif", &[handle, new_owner]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "set_controller → ok");

    {
        let mut other_heap = heap();
        let mut other = context(&mut other_heap, &atoms, foreign);
        let fetched = must(&reg, &mut other, "getStash_nif", &[handle]);
        assert_eq!(
            name_of(&other, fetched),
            Some("empty"),
            "the new owner reaches the stash"
        );
        enqueue(&reg, &mut other, handle, b"abc").expect("the new owner enqueues");
    }
    assert_raise_atom(call(&reg, &mut ctx, "getStash_nif", &[handle]),
        "not_on_controlling_process",
    );

    // A remote pid and a non-pid are `badarg` (the C's `enif_get_local_pid`).
    let remote = remote_pid(&mut ctx);
    assert_badarg(call(&reg, &mut ctx, "set_controller_nif", &[handle, remote]));
    assert_badarg(call(
        &reg,
        &mut ctx,
        "set_controller_nif",
        &[handle, make_small(7)],
    ));
}

/// B.11 — a forged or foreign handle is `badarg`, and `open_nif`'s own answer is the
/// positive control.
#[test]
fn a_forged_handle_is_badarg() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let never_minted = access::make_ref_thing(ctx.heap(), 1).expect("a ref");
    assert_badarg(call(&reg, &mut ctx, "close_nif", &[never_minted]));

    // An id from another of the crate's reference spaces (`src/binary.rs`'s compiled
    // patterns) is not a stream either.
    let foreign_space = access::make_ref_thing(ctx.heap(), 1 << 32).expect("a ref");
    assert_badarg(call(&reg, &mut ctx, "close_nif", &[foreign_space]));

    for bogus in [
        make_small(1),
        ctx.atom("undefined").expect("seeded"),
        NIL,
    ] {
        assert_badarg(call(&reg, &mut ctx, "close_nif", &[bogus]));
        assert_badarg(call(&reg, &mut ctx, "getStash_nif", &[bogus]));
    }

    let handle = open(&reg, &mut ctx);
    assert!(access::is_reference(handle), "open_nif answers a reference");
    assert!(access::is_internal_ref(handle), "and it is an internal ref");
    let id = access::ref_thing_id(handle).expect("an id");
    assert!(
        id >= super::STREAM_ID_BASE,
        "the id comes from the stream space ({id})"
    );
}

/// B.12 — `close_nif` twice is `not_initialized`, and the first close drops the codec, the
/// queue and the stash.
#[test]
fn close_twice_is_not_initialized_and_the_first_close_drops_everything() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, b"abc").expect("enqueue");
    let term = bits::make_binary(ctx.heap(), b"stashed").expect("a binary");
    must(&reg, &mut ctx, "setStash_nif", &[handle, term]);

    let (state, deflate, queue, stash, _) = stream_state(handle);
    assert_eq!(state, State::Deflate);
    assert!(deflate && queue == 1 && stash, "the stream holds its state");

    let ok = must(&reg, &mut ctx, "close_nif", &[handle]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "close → ok");
    let (state, deflate, queue, stash, eos) = stream_state(handle);
    assert_eq!(state, State::Closed, "the tombstone");
    assert!(!deflate, "the compressor is dropped");
    assert_eq!(queue, 0, "the queue is dropped");
    assert!(!stash, "the stash is dropped");
    assert!(!eos);

    assert_raise_atom(call(&reg, &mut ctx, "close_nif", &[handle]), "not_initialized");
    assert_raise_atom(
        deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
        "not_initialized",
    );
}

/// B.13 — an init on an already-initialised stream is `already_initialized`, and the two
/// codecs never share a stream.
#[test]
fn initialising_twice_is_already_initialized() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    assert_raise_atom(
        deflate_init(&reg, &mut ctx, handle, 6, 8, 15, 8, 0),
        "already_initialized",
    );
    assert_raise_atom(
        inflate_init(&reg, &mut ctx, handle, 15, 2),
        "already_initialized",
    );

    // `deflate` on an inflate stream and `inflate` on a deflate stream.
    assert_raise_atom(
        inflate_call(&reg, &mut ctx, handle, INFLATE_IN_CHUNK, INFLATE_OUT_CHUNK, NONE),
        "not_initialized",
    );

    let other = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, other, 15, 2).expect("inflateInit");
    assert_raise_atom(
        deflate_call(&reg, &mut ctx, other, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
        "not_initialized",
    );
    assert_raise_atom(
        deflate_init(&reg, &mut ctx, other, 6, 8, 15, 8, 0),
        "already_initialized",
    );
}

/// B.14 — an impossible `deflateInit2` configuration is `error:stream_error`, not
/// `badarg`: the argument is a valid integer and it is zlib that refuses the value.
#[test]
fn deflate_init_refuses_an_impossible_configuration() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    // windowBits -8: below zlib's minimum window.
    assert_raise_atom(deflate_init(&reg, &mut ctx, handle, 6, 8, -8, 8, 0), "stream_error");
    // A method that is not `Z_DEFLATED`.
    assert_raise_atom(deflate_init(&reg, &mut ctx, handle, 6, 7, 15, 8, 0), "stream_error");
    // A strategy outside 0..=4.
    assert_raise_atom(deflate_init(&reg, &mut ctx, handle, 6, 8, 15, 8, 5), "stream_error");

    // The refusals left the stream untouched, so a legal init still works.
    deflate_init_default(&reg, &mut ctx, handle, 6);
    assert_eq!(
        stream_state(handle).0,
        State::Deflate,
        "a refused init leaves ST_NONE intact"
    );
}

/// B.15 — a handle of the wrong kind is `badarg` even for `deflateInit_nif`, while a fresh
/// stream that was never initialised is `not_initialized` on `deflate_nif/4`.
#[test]
fn deflate_init_on_a_foreign_handle_is_badarg() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let forged = access::make_ref_thing(ctx.heap(), 7).expect("a ref");
    assert_badarg(deflate_init(&reg, &mut ctx, forged, 6, 8, 15, 8, 0));
    assert_badarg(call(&reg, &mut ctx, "getStash_nif", &[forged]));

    let fresh = open(&reg, &mut ctx);
    assert_raise_atom(
        deflate_call(&reg, &mut ctx, fresh, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE),
        "not_initialized",
    );
}

// ===========================================================================
// C. `deflateEnd` / `inflateEnd`
// ===========================================================================

/// C.16 — `deflateEnd` right after `deflateInit`, with no `deflate` call at all, is `ok`
/// (zlib's `INIT_STATE`).
#[test]
fn deflate_end_right_after_init_is_ok() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    let ok = must(&reg, &mut ctx, "deflateEnd_nif", &[handle]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "deflateEnd → ok");
    assert_eq!(stream_state(handle).0, State::New, "back to ST_NONE");
}

/// C.17 — a stream a `deflate` call started but never finished is `error:data_error`, and
/// so is one with a non-empty input queue.
#[test]
fn deflate_end_after_a_deflate_is_data_error() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    // A `none` flush that consumed its input (BUSY_STATE without FINISH_STATE).
    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, b"abc").expect("enqueue");
    let out = answer(deflate_call(&reg, &mut ctx, handle, DEFLATE_CHUNK, DEFLATE_CHUNK, NONE)
            .expect("a none flush"),
    );
    assert_eq!(out.sizes(), vec![2], "the header only");
    assert_raise_atom(call(&reg, &mut ctx, "deflateEnd_nif", &[handle]),
        "data_error",
    );
    assert_eq!(stream_state(handle).0, State::New, "the codec is dropped anyway");

    // A non-empty input queue (`enif_ioq_size(...) > 0`).
    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);
    enqueue(&reg, &mut ctx, handle, b"abc").expect("enqueue");
    assert_raise_atom(call(&reg, &mut ctx, "deflateEnd_nif", &[handle]),
        "data_error",
    );
    assert_eq!(
        stream_state(handle),
        (State::New, false, 0, false, false),
        "the queue is dropped with the codec"
    );
}

/// C.18 — `inflateEnd` is the only place a truncated stream is reported: `data_error`
/// until the stream reached `Z_STREAM_END`, `ok` afterwards, and `data_error` with a
/// non-empty queue.
#[test]
fn inflate_end_splits_on_whether_the_stream_ended() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));
    let image = unhex(F1_ONE_SHOT_LEVEL6);

    // Truncated: 60 of the 102 bytes, so the inflater never sees the trailer.
    let handle = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit");
    enqueue(&reg, &mut ctx, handle, &image[..60]).expect("enqueue");
    let partial = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("a partial inflate"),
    );
    assert!(
        !partial.bytes().is_empty(),
        "the prefix decodes to something, but not to everything"
    );
    assert_raise_atom(call(&reg, &mut ctx, "inflateEnd_nif", &[handle]),
        "data_error",
    );

    // Complete: the whole image reached `Z_STREAM_END`, so `inflateEnd` is `ok`.
    let handle = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit");
    enqueue(&reg, &mut ctx, handle, &image).expect("enqueue");
    let whole = answer(inflate_call(
            &reg,
            &mut ctx,
            handle,
            INFLATE_IN_CHUNK,
            INFLATE_OUT_CHUNK,
            NONE,
        )
        .expect("a whole inflate"),
    );
    assert_eq!(whole.bytes(), f1(), "the whole image decodes");
    assert!(stream_state(handle).4, "Z_STREAM_END was seen");
    let ok = must(&reg, &mut ctx, "inflateEnd_nif", &[handle]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "inflateEnd → ok");

    // A non-empty queue is `data_error` on its own.
    let handle = open(&reg, &mut ctx);
    inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit");
    enqueue(&reg, &mut ctx, handle, &image).expect("enqueue");
    assert_raise_atom(call(&reg, &mut ctx, "inflateEnd_nif", &[handle]),
        "data_error",
    );
}

// ===========================================================================
// D. The stash
// ===========================================================================

/// D.19 — `empty` → `setStash` → `{ok, Term}` → `clearStash` → `empty`, with the retrieved
/// term equal to but **not** the same term as the stored one (it is copied out of the
/// caller's heap as an ETF image).
#[test]
fn the_stash_round_trips_every_term_kind() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    // One term per representation: immediate, boxed scalar, boxed container.
    let binary = bits::make_binary(ctx.heap(), b"abc").expect("a binary");
    let tuple = {
        let one = ctx.int(1).expect("small int");
        let two = ctx.int(2).expect("small int");
        ctx.tuple(&[one, two]).expect("a tuple")
    };
    let list = {
        let one = ctx.int(1).expect("small int");
        let two = ctx.int(2).expect("small int");
        let three = ctx.int(3).expect("small int");
        ctx.list(&[one, two, three]).expect("a list")
    };
    let map = {
        let key = ctx.atom("k").expect("seeded");
        let value = ctx.atom("v").expect("seeded");
        access::make_map(ctx.heap(), &[(key, value)]).expect("a map")
    };
    let float = ctx.make_float(1.5).expect("a float");
    let terms = vec![
        make_small(42),
        ctx.atom("hello").expect("seeded"),
        binary,
        tuple,
        list,
        map,
        float,
    ];
    let kinds = [
        "a small integer",
        "an atom",
        "a binary",
        "a tuple",
        "a list",
        "a map",
        "a float",
    ];

    for (original, kind) in terms.iter().zip(kinds) {
        let handle = open(&reg, &mut ctx);
        deflate_init_default(&reg, &mut ctx, handle, 6);

        let empty = must(&reg, &mut ctx, "getStash_nif", &[handle]);
        assert_eq!(name_of(&ctx, empty), Some("empty"), "{kind}: a fresh stash");
        let ok = must(&reg, &mut ctx, "setStash_nif", &[handle, *original]);
        assert_eq!(name_of(&ctx, ok), Some("ok"), "{kind}: setStash → ok");

        let fetched = must(&reg, &mut ctx, "getStash_nif", &[handle]);
        assert!(
            access::is_tuple(fetched) && access::tuple_size(fetched) == 2,
            "{kind}: getStash answers {{ok, Term}}"
        );
        assert_eq!(
            name_of(&ctx, access::tuple_element(fetched, 0)),
            Some("ok"),
            "{kind}: the `ok` tag"
        );
        let retrieved = access::tuple_element(fetched, 1);
        assert!(
            beam_term::compare::eq(*original, retrieved, ctx.atoms()),
            "{kind}: the retrieved term equals the stored one"
        );
        if access::is_small(*original) || access::is_atom(*original) {
            assert_eq!(*original, retrieved, "{kind}: an immediate is its own copy");
        } else {
            assert_ne!(
                *original, retrieved,
                "{kind}: the term is copied out of the caller's heap, so it cannot be the \
                 same word"
            );
        }

        let ok = must(&reg, &mut ctx, "clearStash_nif", &[handle]);
        assert_eq!(name_of(&ctx, ok), Some("ok"), "{kind}: clearStash → ok");
        let empty = must(&reg, &mut ctx, "getStash_nif", &[handle]);
        assert_eq!(name_of(&ctx, empty), Some("empty"), "{kind}: the stash is gone");
    }
}

/// D.20 — a second `setStash` and a `clearStash` of an empty stash are the **bare atom**
/// `error` (`erlang:error(error)`), not a tuple.
#[test]
fn a_second_stash_and_an_empty_clear_are_the_bare_atom_error() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);

    assert_raise_atom(call(&reg, &mut ctx, "clearStash_nif", &[handle]), "error");

    let term = bits::make_binary(ctx.heap(), b"abc").expect("a binary");
    must(&reg, &mut ctx, "setStash_nif", &[handle, term]);
    assert_raise_atom(call(&reg, &mut ctx, "setStash_nif", &[handle, term]), "error");

    let ok = must(&reg, &mut ctx, "clearStash_nif", &[handle]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "the first clear works");
    assert_raise_atom(call(&reg, &mut ctx, "clearStash_nif", &[handle]), "error");
}

/// D.21 — every reset, end and close drops an existing stash.
#[test]
fn every_reset_and_end_drops_the_stash() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let stash = |ctx: &mut BifContext<'_>| {
        let segment = bits::make_binary(ctx.heap(), b"abc").expect("a binary");
        let list = ctx.list(&[segment]).expect("a list");
        let tag = ctx.atom("deflate").expect("seeded");
        ctx.tuple(&[tag, list]).expect("the stash tuple")
    };

    for key in [
        "deflateReset_nif",
        "inflateReset_nif",
        "deflateEnd_nif",
        "inflateEnd_nif",
        "close_nif",
    ] {
        let handle = open(&reg, &mut ctx);
        let deflate_side = key.starts_with("deflate");
        if deflate_side {
            deflate_init_default(&reg, &mut ctx, handle, 6);
        } else if key.starts_with("inflate") {
            inflate_init(&reg, &mut ctx, handle, 15, 2).expect("inflateInit");
        }
        let term = stash(&mut ctx);
        must(&reg, &mut ctx, "setStash_nif", &[handle, term]);
        assert!(
            call(&reg, &mut ctx, "getStash_nif", &[handle]).is_ok(),
            "{key}: the stash is set"
        );

        must(&reg, &mut ctx, key, &[handle]);
        if key == "close_nif" {
            // A closed stream is unreachable through the NIFs, so the drop is read from
            // the registry itself.
            assert!(!stream_state(handle).3, "{key}: the stash is dropped");
        } else {
            let after = must(&reg, &mut ctx, "getStash_nif", &[handle]);
            assert_eq!(
                name_of(&ctx, after),
                Some("empty"),
                "{key}: the stash is dropped"
            );
        }
    }
}

/// D.22 — the stash carries `{deflate, Output}` exactly, which is its only production
/// shape (`save_progress/3`).
#[test]
fn the_stash_carries_the_production_shape() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);

    let payload = {
        let binary = bits::make_binary(ctx.heap(), b"abc").expect("a binary");
        let one = ctx.int(1).expect("small int");
        let two = ctx.int(2).expect("small int");
        let three = ctx.int(3).expect("small int");
        ctx.list(&[binary, one, two, three]).expect("an iolist")
    };
    let tag = ctx.atom("deflate").expect("seeded");
    let progress = ctx.tuple(&[tag, payload]).expect("a tuple");

    let ok = must(&reg, &mut ctx, "setStash_nif", &[handle, progress]);
    assert_eq!(name_of(&ctx, ok), Some("ok"), "setStash → ok");

    let fetched = must(&reg, &mut ctx, "getStash_nif", &[handle]);
    let retrieved = access::tuple_element(fetched, 1);
    assert!(
        beam_term::compare::eq(progress, retrieved, ctx.atoms()),
        "the {{deflate, iolist}} image round-trips"
    );
    assert_eq!(
        name_of(&ctx, access::tuple_element(retrieved, 0)),
        Some("deflate"),
        "the kind tag survives"
    );
    let output = access::tuple_element(retrieved, 1);
    let leaves = list_bytes(output);
    assert_eq!(leaves.len(), 4, "the iolist keeps its four elements");
    assert_eq!(leaves[0], b"abc".to_vec(), "the binary segment");
    assert_eq!(
        leaves[1..].iter().map(|leaf| leaf.len()).sum::<usize>(),
        0,
        "the small integers are not binaries"
    );
}

// ===========================================================================
// E. Representation pins
// ===========================================================================

/// E.23 — `enqueue_nif` accepts only iodata, is `ok` for an empty iovec, and hands back
/// exactly the segments past the 256-segment cap.
#[test]
fn enqueue_refuses_non_iodata_and_caps_the_segment_count() {
    let reg = registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = context(&mut heap, &atoms, ProcessId::new(1, 0));

    let handle = open(&reg, &mut ctx);
    deflate_init_default(&reg, &mut ctx, handle, 6);

    // A bare integer, an atom and a non-byte-aligned bitstring are all `badarg`.
    for bogus in [
        make_small(7),
        ctx.atom("hello").expect("seeded"),
        bits::make_bitstring(ctx.heap(), &[0b1010_0000], 3).expect("a bitstring"),
    ] {
        assert_badarg(call(&reg, &mut ctx, "enqueue_nif", &[handle, bogus]));
    }

    // `[]` — `enqueue_input(Z, [])` — is `ok`.
    let ok = enqueue(&reg, &mut ctx, handle, b"").expect("an empty iovec");
    assert_eq!(name_of(&ctx, ok), Some("ok"), "an empty iovec is ok");
    assert_eq!(stream_state(handle).2, 0, "and it queues nothing");

    // 300 one-byte segments: the first 256 are taken, the rest come back as
    // `{continue, Tail}`.
    let segments = 300usize;
    let mut terms = Vec::with_capacity(segments);
    for index in 0..segments {
        terms.push(bits::make_binary(ctx.heap(), &[index as u8]).expect("a binary"));
    }
    let iovec = ctx.list(&terms).expect("a list");
    let first = call(&reg, &mut ctx, "enqueue_nif", &[handle, iovec]).expect("enqueue");
    assert!(
        access::is_tuple(first) && access::tuple_size(first) == 2,
        "a partial enqueue answers {{continue, Tail}}"
    );
    assert_eq!(
        name_of(&ctx, access::tuple_element(first, 0)),
        Some("continue"),
        "the continue tag"
    );
    let mut tail = access::tuple_element(first, 1);
    assert_eq!(stream_state(handle).2, 256, "the 256-segment cap");
    let mut remaining = 0usize;
    {
        let mut rest = tail;
        while !access::is_nil(rest) {
            assert!(access::is_list(rest), "the tail is a proper list");
            remaining += 1;
            rest = access::cdr(rest);
        }
    }
    assert_eq!(remaining, segments - 256, "the tail is exactly the remainder");
    assert_eq!(
        access::car(tail),
        terms[256],
        "the tail starts at the 257th segment"
    );

    // Feeding the tail in a loop terminates, and every segment lands in the queue.
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds <= 4, "the enqueue loop terminates");
        let answer = call(&reg, &mut ctx, "enqueue_nif", &[handle, tail]).expect("enqueue");
        if access::is_tuple(answer) {
            tail = access::tuple_element(answer, 1);
        } else {
            assert_eq!(name_of(&ctx, answer), Some("ok"), "the loop ends in ok");
            break;
        }
    }
    let (_, _, queued, _, _) = stream_state(handle);
    assert_eq!(queued, segments, "every segment is queued");
}
