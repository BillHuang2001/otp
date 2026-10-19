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
//! **Live-oracle** cross-checks for the decompression direction: every assertion here
//! compares this crate against the system `erl`'s `zlib` NIF (`zlib_nif.c` on zlib
//! 1.3.2), not against a pasted golden.
//!
//! Two observables of `inflate()` are exposed to Erlang, and both are pinned here:
//!
//! * **The per-call chunking.** `zlib:safeInflate/2` is exactly one
//!   `inflate_nif(Z, ?INFLATE_IN_CHUNKSIZE, ?INFLATE_OUT_CHUNKSIZE, Z_NO_FLUSH)` call, so
//!   its `{continue, Out}` / `{finished, Out}` sequence *is* the C's `avail_out`
//!   accounting. `zlib.erl` hardcodes the output buffer at
//!   `?INFLATE_OUT_CHUNKSIZE` (16 KiB; "chunk sizes are hardcoded on account of them
//!   screwing with the predictability of the system"), so 16 KiB is the only chunk size
//!   the oracle can be driven at — hence the 1:1 sequence comparison in
//!   [`the_safeinflate_chunk_sequence_matches_the_oracle`] and the weaker, documented
//!   property asserted for the finer buffers in
//!   [`every_output_buffer_size_delivers_the_oracles_plaintext`].
//! * **How much is decodable from a prefix of the stream.** For every prefix length,
//!   `zlib:inflate/2` returns everything that prefix can produce, which pins the
//!   bit-level state machine at *every* input boundary — see
//!   [`the_per_prefix_production_counts_match_the_oracle`]. This is the fine-grained
//!   substitute for the per-call chunking that `safeInflate/2` cannot report (a
//!   1-byte-at-a-time `safeInflate` feed collapses to a single call, because the NIF
//!   reports `finished` as soon as its *input queue* drains, whatever the stream state).
//!
//! The rest of the file covers the preset-dictionary protocol, the four framings and
//! `reset`/`reset_with_window_bits` reuse, all against the oracle's own images and its
//! own decode of them.
//!
//! Every `erl` invocation happens under [`zsupport::oracle_lock`], and when there is no
//! usable `erl` each test prints one `SKIP …` line and returns — CI without an Erlang
//! installation stays green.

mod zsupport;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use beam_deflate::{crc32, Flush, Inflate, Status};
use zsupport::{oracle_batch, unhex, Case};

/// zlib / RFC 1950 (`zlib:compress/1`).
const ZLIB: i8 = 15;
/// raw deflate / RFC 1951 (`zlib:zip/1`).
const RAW: i8 = -15;
/// gzip / RFC 1952 (`zlib:gzip/1`).
const GZIP: i8 = 16 + 15;
/// auto-detect zlib or gzip (`inflateInit(Z, 32 + 15)`).
const AUTO: i8 = 32 + 15;

/// `zlib.erl`'s `?INFLATE_OUT_CHUNKSIZE` — the only output chunk size the oracle can be
/// driven at.
const ORACLE_OUT_CHUNK: usize = 16 * 1024;

/// A generous output ceiling for the one-shot assertions (the largest body is 40 KiB).
const BIG: usize = 1 << 20;

// ---------------------------------------------------------------------------
// The bodies, mirrored in Erlang inside `FIXTURE_SCRIPT`
// ---------------------------------------------------------------------------

/// 40 KiB of `"abcdefgh"`, compressed to ~90 bytes: every match comes out of the window,
/// so this body exercises `inflate_fast` and the window's slide.
fn rep40k() -> Vec<u8> {
    b"abcdefgh".repeat(5120)
}

/// 40 000 bytes of English-ish text: compressible, but with long literal runs, so the
/// produced-per-call curve is not a single repeated match.
fn text40k() -> Vec<u8> {
    let mut body = b"the quick brown fox jumps over the lazy dog ".repeat(1000);
    body.truncate(40_000);
    body
}

/// The bodies the fixture script compresses, with the tag it labels them by.
fn bodies() -> Vec<(&'static str, Vec<u8>)> {
    vec![("r40k", rep40k()), ("t40k", text40k())]
}

// ---------------------------------------------------------------------------
// The oracle fixture: one `erl` boot for everything the tests below need
// ---------------------------------------------------------------------------

/// One `erl` boot that compresses both bodies into all three framings and records
/// everything the tests compare against, as `key=value` lines.
///
/// * `<tag>_{zlib,raw,gzip}` — the oracle's own byte image of the body.
/// * `<tag>_{len,crc}` — the body's length and CRC-32 (the oracle's own arithmetic).
/// * `<tag>_<framing>_dec_{len,crc}` — the oracle's own decode of that image, through
///   `zlib:inflate/2` under the framing named by the key (`autozlib`/`autogzip` are
///   windowBits 47).
/// * `<tag>_chunk_{kinds,sizes}` — the `zlib:safeInflate/2` call sequence over the zlib
///   image: `continue`/`finished` per call and the bytes each call returned.
/// * `<tag>_census_<framing>` — for every prefix length `0..=len`, the number of bytes
///   that prefix decodes to.
/// * `<tag>_{half,notrailer}_{len,crc,dict_len,dict_crc}` — a partial decode's output and
///   `zlib:inflateGetDictionary/1` (half the image; the image without its Adler-32
///   trailer).
const FIXTURE_SCRIPT: &str = r#"
P = fun(K,V) -> io:format("~s=~s~n",[K,V]) end,
Comp = fun(Data,WB) -> Z=zlib:open(), ok=zlib:deflateInit(Z,default,deflated,WB,8,default),
        S=iolist_to_binary(zlib:deflate(Z,Data,finish)), ok=zlib:deflateEnd(Z), zlib:close(Z), S end,
Infl = fun(S,WB) -> Z=zlib:open(), ok=zlib:inflateInit(Z,WB),
        O=iolist_to_binary(zlib:inflate(Z,S)), ok=zlib:inflateEnd(Z), zlib:close(Z), O end,
Part = fun(S,WB,N) -> Z=zlib:open(), ok=zlib:inflateInit(Z,WB),
        O=iolist_to_binary(zlib:inflate(Z,binary:part(S,0,N),[{exception_on_need_dict,false}])),
        Di=zlib:inflateGetDictionary(Z), ok=zlib:close(Z), {O,Di} end,
PartP = fun(Tag,Suf,{O,Di}) ->
        P(Tag++"_"++Suf++"_len", integer_to_list(byte_size(O))),
        P(Tag++"_"++Suf++"_crc", integer_to_list(erlang:crc32(O))),
        P(Tag++"_"++Suf++"_dict_len", integer_to_list(byte_size(Di))),
        P(Tag++"_"++Suf++"_dict_crc", integer_to_list(erlang:crc32(Di))) end,
Chunks = fun(S,WB) -> Z=zlib:open(), ok=zlib:inflateInit(Z,WB),
        F=fun R(D,Acc) -> case zlib:safeInflate(Z,D) of
                {continue,O} -> R([],[{continue,iolist_size(O)}|Acc]);
                {finished,O} -> lists:reverse([{finished,iolist_size(O)}|Acc]) end end,
        Res=F(S,[]), ok=zlib:inflateEnd(Z), zlib:close(Z), Res end,
Census = fun(S,WB) -> N=byte_size(S),
        F=fun R(Q,Acc) when Q>N -> lists:reverse(Acc);
             R(Q,Acc) ->
                Z=zlib:open(), ok=zlib:inflateInit(Z,WB),
                O=iolist_to_binary(zlib:inflate(Z,binary:part(S,0,Q),[{exception_on_need_dict,false}])),
                ok=zlib:close(Z), R(Q+1,[byte_size(O)|Acc]) end,
        lists:join($,, [integer_to_list(X) || X <- F(0,[])]) end,
lists:foreach(fun({Tag,Data}) ->
    Zl=Comp(Data,15), Rw=Comp(Data,-15), Gz=Comp(Data,31),
    P(Tag++"_zlib", binary:encode_hex(Zl)),
    P(Tag++"_raw", binary:encode_hex(Rw)),
    P(Tag++"_gzip", binary:encode_hex(Gz)),
    P(Tag++"_len", integer_to_list(byte_size(Data))),
    P(Tag++"_crc", integer_to_list(erlang:crc32(Data))),
    lists:foreach(fun({Suf,S,WB}) ->
        O=Infl(S,WB),
        P(Tag++"_"++Suf++"_dec_len", integer_to_list(byte_size(O))),
        P(Tag++"_"++Suf++"_dec_crc", integer_to_list(erlang:crc32(O)))
      end, [{"zlib",Zl,15},{"raw",Rw,-15},{"gzip",Gz,31},{"autozlib",Zl,47},{"autogzip",Gz,47}]),
    CH=Chunks(Zl,15),
    P(Tag++"_chunk_kinds", lists:join($,, [atom_to_list(K) || {K,_} <- CH])),
    P(Tag++"_chunk_sizes", lists:join($,, [integer_to_list(N) || {_,N} <- CH])),
    P(Tag++"_census_zlib", Census(Zl,15)),
    P(Tag++"_census_raw", Census(Rw,-15)),
    P(Tag++"_census_gzip", Census(Gz,31)),
    PartP(Tag,"half",Part(Zl,15,byte_size(Zl) div 2)),
    PartP(Tag,"notrailer",Part(Zl,15,byte_size(Zl)-4))
  end, [{"r40k", binary:copy(<<"abcdefgh">>,5120)},
        {"t40k", binary:part(binary:copy(<<"the quick brown fox jumps over the lazy dog ">>,1000),0,40000)}]),
halt().
"#;

/// The fixture map, or `None` (after printing the skip line) when there is no oracle.
///
/// The script runs **once** per test binary: it is the single `erl` boot behind every
/// chunking/census/dictionary assertion here.
fn fixtures() -> Option<&'static BTreeMap<String, String>> {
    static FIXTURES: OnceLock<Option<BTreeMap<String, String>>> = OnceLock::new();
    let fixtures = FIXTURES.get_or_init(|| {
        if !zsupport::have_oracle() {
            return None;
        }
        let text = zsupport::erl_eval(FIXTURE_SCRIPT).expect("the oracle fixture script ran");
        let mut map = BTreeMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        Some(map)
    });
    if fixtures.is_none() {
        println!("SKIP inflate oracle: erl not found");
    }
    fixtures.as_ref()
}

/// One `key=value` fixture value, as text.
fn text<'a>(fixtures: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    fixtures
        .get(key)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("the oracle fixture script did not emit {key}"))
}

/// One fixture value as bytes (every byte-image value is hex).
fn image(fixtures: &BTreeMap<String, String>, key: &str) -> Vec<u8> {
    unhex(text(fixtures, key))
}

/// One fixture value as a number.
fn number(fixtures: &BTreeMap<String, String>, key: &str) -> usize {
    text(fixtures, key)
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number"))
}

/// One fixture value as a comma-separated list of numbers.
fn numbers(fixtures: &BTreeMap<String, String>, key: &str) -> Vec<usize> {
    text(fixtures, key)
        .split(',')
        .map(|item| item.parse().unwrap_or_else(|_| panic!("{key}: {item:?} is not a number")))
        .collect()
}

// ---------------------------------------------------------------------------
// The pump, instrumented the way `safeInflate/2` observes it
// ---------------------------------------------------------------------------

/// One instrumented decode: the body, every call's `(status, consumed, produced)`, and
/// the number of input bytes consumed by the time the stream ended (or stalled).
#[derive(Debug)]
struct Run {
    body: Vec<u8>,
    calls: Vec<(Status, usize, usize)>,
    consumed: usize,
}

impl Run {
    /// The statuses in `zlib.erl`'s vocabulary: `Z_STREAM_END` is `finished`, anything
    /// else `safeInflate/2` can return is `continue`.
    fn kinds(&self) -> Vec<&'static str> {
        self.calls
            .iter()
            .map(|(status, _, _)| match status {
                Status::StreamEnd => "finished",
                _ => "continue",
            })
            .collect()
    }

    /// The bytes each call produced, in order — `byte_size(Out)` per `safeInflate`.
    fn sizes(&self) -> Vec<usize> {
        self.calls.iter().map(|(_, _, produced)| *produced).collect()
    }
}

/// Pump `image` through a fresh stream in `in_chunk`-sized input slices and
/// `out_chunk`-sized output slices until `Z_STREAM_END` (or until a call makes no
/// progress, which is what a partial feed ends with).
///
/// The first call gets `image[0..in_chunk]` and each later one resumes at the first
/// unconsumed byte, so `in_chunk == image.len()` is the "all the input is available"
/// route and `in_chunk == 1` is one byte at a time.
fn pump(window_bits: i8, image: &[u8], in_chunk: usize, out_chunk: usize) -> Run {
    let mut stream = Inflate::new(window_bits).expect("the window bits are a fixture");
    let mut buf = vec![0u8; out_chunk];
    let mut body = Vec::new();
    let mut calls = Vec::new();
    let mut consumed = 0usize;
    loop {
        let end = (consumed + in_chunk).min(image.len());
        let step = stream
            .inflate(&image[consumed..end], &mut buf, Flush::None)
            .expect("a stream-level outcome is a status, never an error");
        consumed += step.consumed;
        body.extend_from_slice(&buf[..step.produced]);
        calls.push((step.status, step.consumed, step.produced));
        assert!(calls.len() < 1_000_000, "the pump must terminate");
        match step.status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                if step.consumed == 0 && step.produced == 0 {
                    // No progress: the input ran out mid-stream (a partial feed), or the
                    // stream needs bytes this feed does not contain.
                    break;
                }
            }
            other => panic!("the pump reported {other:?} after {consumed} input bytes"),
        }
    }
    Run {
        body,
        calls,
        consumed,
    }
}

/// Everything `prefix` decodes to, in one stream with all of it available — the shape
/// `zlib:inflate(Z, Prefix)` has from the outside.
fn decodable(window_bits: i8, prefix: &[u8]) -> usize {
    pump(window_bits, prefix, prefix.len().max(1), BIG).body.len()
}

// ---------------------------------------------------------------------------
// The per-call chunk boundaries (`zlib:safeInflate/2`)
// ---------------------------------------------------------------------------

/// `safeInflate/2`'s call sequence — the bytes each call returns and whether the call
/// that ended it said `finished` — is reproduced call for call.
///
/// This is the tightest chunk-boundary comparison the oracle admits: `zlib.erl` hardcodes
/// the output buffer at 16 KiB, so a 16 KiB output slice on this side makes every call
/// carry exactly the same `avail_out` as the NIF's, and zlib fills that buffer unless the
/// input or the stream's end says otherwise. A decoder that reported a different
/// `produced`, or kept going after `Z_STREAM_END`, would diverge here.
#[test]
fn the_safeinflate_chunk_sequence_matches_the_oracle() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        let compressed = image(fixtures, &format!("{tag}_zlib"));
        assert!(compressed.len() > 8, "{tag}: a real stream");
        let run = pump(ZLIB, &compressed, compressed.len(), ORACLE_OUT_CHUNK);
        assert_eq!(run.body, plain, "{tag}: the plaintext");
        assert_eq!(run.consumed, compressed.len(), "{tag}: every byte is consumed");

        assert_eq!(
            run.kinds(),
            text(fixtures, &format!("{tag}_chunk_kinds")).split(',').collect::<Vec<_>>(),
            "{tag}: the continue/finished sequence"
        );
        let sizes = run.sizes();
        assert_eq!(
            sizes,
            numbers(fixtures, &format!("{tag}_chunk_sizes")),
            "{tag}: the bytes each safeInflate call returned"
        );

        // And the shape that sequence must have: `ceil(total / 16 KiB)` calls, every one
        // of which fills the 16 KiB buffer except the last, which carries the remainder.
        let total = plain.len();
        let expected_calls = total.div_ceil(ORACLE_OUT_CHUNK);
        assert_eq!(sizes.len(), expected_calls, "{tag}: the call count");
        for (index, produced) in sizes.iter().enumerate() {
            let want = if index + 1 == expected_calls {
                total - ORACLE_OUT_CHUNK * (expected_calls - 1)
            } else {
                ORACLE_OUT_CHUNK
            };
            assert_eq!(*produced, want, "{tag}: call {index} of {expected_calls}");
        }
        // Only the last call sees the stream end.
        let ends: Vec<usize> = run
            .kinds()
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == "finished")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(ends, vec![expected_calls - 1], "{tag}: Z_STREAM_END happens once");
    }
}

/// A 1-byte, 7-byte or 1 KiB output buffer changes the *chunking* and nothing else.
///
/// The oracle cannot be driven at these sizes — `?INFLATE_OUT_CHUNKSIZE` is compiled into
/// `zlib.erl` — so the exact property asserted is the one that survives that: for any
/// output buffer the plaintext, the total consumed count and the status of the call that
/// ends the stream are the same, and the per-call `produced` sequence is the unique
/// chunking of that plaintext into buffer-sized pieces (every call fills the buffer but
/// the last). The plaintext itself is the oracle's: its length and CRC-32 are the ones
/// the oracle's own decode of the same image reports.
#[test]
fn every_output_buffer_size_delivers_the_oracles_plaintext() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        let compressed = image(fixtures, &format!("{tag}_zlib"));
        let oracle_len = number(fixtures, &format!("{tag}_zlib_dec_len"));
        let oracle_crc = number(fixtures, &format!("{tag}_zlib_dec_crc"));
        assert_eq!(oracle_len, plain.len(), "{tag}: the oracle's decode is this body");
        assert_eq!(oracle_crc, crc32(0, &plain) as usize, "{tag}: the oracle's CRC-32");

        for out_chunk in [1usize, 7, 1024, ORACLE_OUT_CHUNK] {
            let run = pump(ZLIB, &compressed, compressed.len(), out_chunk);
            assert_eq!(run.body, plain, "{tag}: {out_chunk}-byte output buffer");
            assert_eq!(crc32(0, &run.body) as usize, oracle_crc, "{tag}/{out_chunk}: CRC-32");
            assert_eq!(run.consumed, compressed.len(), "{tag}/{out_chunk}: consumed");

            let sizes = run.sizes();
            let expected_calls = plain.len().div_ceil(out_chunk);
            assert_eq!(
                sizes.len(),
                expected_calls,
                "{tag}/{out_chunk}: a full buffer is always filled"
            );
            for (index, produced) in sizes.iter().enumerate() {
                let want = if index + 1 == expected_calls {
                    plain.len() - out_chunk * (expected_calls - 1)
                } else {
                    out_chunk
                };
                assert_eq!(*produced, want, "{tag}/{out_chunk}: call {index}");
                assert!(*produced <= out_chunk, "{tag}/{out_chunk}: never past the buffer");
            }
            assert_eq!(run.kinds().last(), Some(&"finished"), "{tag}/{out_chunk}");
        }
    }
}

// ---------------------------------------------------------------------------
// The bit-level state machine, pinned at every input boundary
// ---------------------------------------------------------------------------

/// For **every prefix** of every oracle image, the number of bytes that prefix decodes to
/// is the oracle's number.
///
/// This is the fine-grained check the per-call chunking cannot give: it pins the decoder's
/// bit accounting at every input boundary (each prefix is one `zlib:inflate/2` on the
/// oracle's side), across all three framings, so a one-bit divergence in a Huffman decode,
/// a stored-block header or a trailer check shows up as a difference in the curve rather
/// than only at the end. Larger prefixes must also never decode to less than smaller ones.
#[test]
fn the_per_prefix_production_counts_match_the_oracle() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        for (framing, window_bits) in [("zlib", ZLIB), ("raw", RAW), ("gzip", GZIP)] {
            let compressed = image(fixtures, &format!("{tag}_{framing}"));
            let census = numbers(fixtures, &format!("{tag}_census_{framing}"));
            assert_eq!(
                census.len(),
                compressed.len() + 1,
                "{tag}/{framing}: one count per prefix, including the empty one"
            );
            assert_eq!(census[0], 0, "{tag}/{framing}: an empty prefix decodes to nothing");
            assert_eq!(
                *census.last().expect("a census"),
                plain.len(),
                "{tag}/{framing}: the whole image decodes to the body"
            );
            for (index, want) in census.iter().enumerate() {
                let got = decodable(window_bits, &compressed[..index]);
                assert_eq!(
                    got, *want,
                    "{tag}/{framing}: prefix {index} of {} decodes to {got} bytes, the oracle says {want}",
                    compressed.len()
                );
                if index > 0 {
                    assert!(
                        got >= census[index - 1],
                        "{tag}/{framing}: prefix {index} produced less than prefix {}",
                        index - 1
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Input chunking is invisible; the per-call sequence is not
// ---------------------------------------------------------------------------

/// Feeding the stream one byte, three bytes, a kilobyte or all at once produces the same
/// plaintext and ends at the same input position.
///
/// The per-call `produced` sequence *does* depend on the input chunking — that is a
/// property of `inflate()` itself (`Z_OK` with zero output is normal while a symbol is
/// still incomplete) — so this test asserts both halves: the plaintext and the
/// `Z_STREAM_END` position are invariant, and the fine-grained sequences are not, which is
/// why the chunk-boundary comparison above is done at the oracle's own chunk size.
#[test]
fn the_plaintext_and_the_end_position_do_not_depend_on_the_input_chunking() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        let compressed = image(fixtures, &format!("{tag}_zlib"));
        let whole = pump(ZLIB, &compressed, compressed.len(), ORACLE_OUT_CHUNK);
        assert_eq!(whole.body, plain);
        let mut sequences = vec![whole.sizes()];
        for in_chunk in [1usize, 3, 1024] {
            let run = pump(ZLIB, &compressed, in_chunk, ORACLE_OUT_CHUNK);
            assert_eq!(run.body, plain, "{tag}: {in_chunk}-byte input slices");
            assert_eq!(
                run.consumed,
                compressed.len(),
                "{tag}/{in_chunk}: the stream ends at the last input byte"
            );
            assert_eq!(
                run.calls.last().map(|call| call.0),
                Some(Status::StreamEnd),
                "{tag}/{in_chunk}: Z_STREAM_END is the last call"
            );
            assert_eq!(
                run.sizes().iter().sum::<usize>(),
                plain.len(),
                "{tag}/{in_chunk}: the produced total"
            );
            sequences.push(run.sizes());
        }
        assert!(
            sequences.windows(2).any(|pair| pair[0] != pair[1]),
            "{tag}: the per-call sequence should depend on the input chunking"
        );
    }
}

// ---------------------------------------------------------------------------
// The preset-dictionary protocol
// ---------------------------------------------------------------------------

/// The dictionary the deflate side is primed with (`deflateSetDictionary/2`).
const PRESET_DICT: &[u8] = b"the quick brown fox jumps over the lazy dog";

/// The body compressed against [`PRESET_DICT`]: `"the quick brown fox …"` 20 times.
fn dictionary_body() -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog ".repeat(20)
}

/// `deflateSetDictionary` → `deflate` on one side, `inflate` → `need_dictionary` →
/// `inflateSetDictionary` → `inflate` on the other, in one boot.
const DICTIONARY_SCRIPT: &str = r#"
P = fun(K,V) -> io:format("~s=~s~n",[K,V]) end,
Dict = <<"the quick brown fox jumps over the lazy dog">>,
Data = binary:copy(<<"the quick brown fox jumps over the lazy dog ">>,20),
Z0 = zlib:open(), ok = zlib:deflateInit(Z0,9,deflated,15,8,default),
DefAdler = zlib:deflateSetDictionary(Z0,Dict),
S = iolist_to_binary(zlib:deflate(Z0,Data,finish)),
ok = zlib:deflateEnd(Z0), zlib:close(Z0),
P("stream", binary:encode_hex(S)),
P("def_adler", integer_to_list(DefAdler)),
P("data_len", integer_to_list(byte_size(Data))),
P("data_crc", integer_to_list(erlang:crc32(Data))),
Z = zlib:open(), ok = zlib:inflateInit(Z,15),
R = zlib:inflate(Z,S,[{exception_on_need_dict,false}]),
case R of
    {need_dictionary, Adler, Out} ->
        P("infl_adler", integer_to_list(Adler)),
        P("need_len", integer_to_list(iolist_size(Out))),
        ok = zlib:inflateSetDictionary(Z,Dict),
        Rest = iolist_to_binary(zlib:inflate(Z,[])),
        P("resumed_len", integer_to_list(byte_size(Rest))),
        P("resumed_crc", integer_to_list(erlang:crc32(Rest))),
        ResumedDict = zlib:inflateGetDictionary(Z),
        P("resumed_dict_len", integer_to_list(byte_size(ResumedDict))),
        P("resumed_dict_crc", integer_to_list(erlang:crc32(ResumedDict))),
        ok = zlib:inflateEnd(Z);
    _ ->
        P("infl_adler", "none"), P("need_len", "none"),
        P("resumed_len", "none"), P("resumed_crc", "none"),
        P("resumed_dict_len", "none"), P("resumed_dict_crc", "none")
end,
zlib:close(Z),
halt().
"#;

/// A stream compressed against a preset dictionary reports `Z_NEED_DICT` with the
/// dictionary's Adler-32 — the oracle's own number, from both sides of the protocol — and
/// decodes once that dictionary is installed.
#[test]
fn a_preset_dictionary_stream_needs_the_oracles_adler_and_then_decodes() {
    if !zsupport::have_oracle() {
        println!("SKIP inflate oracle: erl not found");
        return;
    }
    let script = zsupport::erl_eval(DICTIONARY_SCRIPT).expect("the dictionary script ran");
    let mut fixtures = BTreeMap::new();
    for line in script.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fixtures.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let compressed = image(&fixtures, "stream");
    let body = dictionary_body();
    assert_eq!(number(&fixtures, "data_len"), body.len());
    assert_eq!(number(&fixtures, "data_crc"), crc32(0, &body) as usize);

    // The header's DICTID is the dictionary's Adler-32; the oracle reports it from the
    // deflate side (`deflateSetDictionary`'s return value) and from the inflate side
    // (`{need_dictionary, Adler, _}`), and they agree with the checksum function here.
    let adler = beam_deflate::adler32(1, PRESET_DICT) as usize;
    assert_eq!(number(&fixtures, "def_adler"), adler, "the deflate side's Adler-32");
    assert_eq!(number(&fixtures, "infl_adler"), adler, "the inflate side's Adler-32");
    assert_eq!(number(&fixtures, "need_len"), 0, "nothing is decodable before the dictionary");

    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    let mut buf = vec![0u8; BIG];
    let step = stream
        .inflate(&compressed, &mut buf, Flush::None)
        .expect("a status, not an error");
    assert_eq!(
        step.status,
        Status::NeedDict(adler as u32),
        "the stream must ask for the oracle's dictionary id"
    );
    assert_eq!(step.produced, 0, "no output before the dictionary");
    assert!(!stream.at_stream_end());

    // A dictionary that does not hash to the id in the header is refused, and refusing it
    // leaves the stream waiting rather than corrupting it.
    assert!(
        stream.set_dictionary(b"not the fox").is_err(),
        "the wrong dictionary must be refused"
    );
    stream
        .set_dictionary(PRESET_DICT)
        .expect("the oracle's dictionary must be accepted");
    // `inflateSetDictionary` seeds the window with the dictionary itself.
    assert_eq!(stream.get_dictionary(), PRESET_DICT);

    let mut produced = Vec::new();
    let mut consumed = step.consumed;
    loop {
        let step = stream
            .inflate(&compressed[consumed..], &mut buf, Flush::None)
            .expect("pump");
        consumed += step.consumed;
        produced.extend_from_slice(&buf[..step.produced]);
        match step.status {
            Status::StreamEnd => break,
            Status::Ok => {}
            other => panic!("unexpected {other:?} after the dictionary"),
        }
    }
    assert_eq!(produced, body, "the resumed stream decodes the oracle's body");
    assert_eq!(consumed, compressed.len());
    assert_eq!(
        produced.len(),
        number(&fixtures, "resumed_len"),
        "the oracle's resumed length"
    );
    assert_eq!(
        crc32(0, &produced) as usize,
        number(&fixtures, "resumed_crc"),
        "the oracle's resumed CRC-32"
    );
    assert!(stream.at_stream_end());
    // The window now holds what the oracle's window holds. For this shape that is still
    // the preset dictionary itself: the call that finished the stream also verified the
    // Adler-32 trailer, and zlib's `CHECK` state re-bases its output counter before
    // `inf_leave` hands the byte count to `updatewindow`, so that call contributes
    // nothing to the window. `zlib:inflateGetDictionary/1` reports the same 43 bytes on
    // the oracle side, which is what is compared here.
    let dictionary = stream.get_dictionary();
    assert_eq!(
        dictionary.len(),
        number(&fixtures, "resumed_dict_len"),
        "the oracle's window length after the resume"
    );
    assert_eq!(
        crc32(0, dictionary) as usize,
        number(&fixtures, "resumed_dict_crc"),
        "the oracle's window bytes after the resume"
    );
    assert_eq!(dictionary, PRESET_DICT, "the trailer re-base leaves the window alone");
}

// ---------------------------------------------------------------------------
// The framings, the small corpus, and `reset` reuse
// ---------------------------------------------------------------------------

/// The oracle's own images decode under their own framing *and* under auto-detect, and
/// `reset` starts a second stream in the same handle.
///
/// The reference for every plaintext is the oracle's decode of the same image (its length
/// and CRC-32), so a divergence cannot hide behind a shared generator. The tail of the
/// test sweeps `corpus_small` through `oracle_batch` so the tiny shapes (an empty body, a
/// struck literal run, a body longer than the window) are covered live, not only as
/// pasted goldens.
#[test]
fn the_oracles_framings_decode_and_a_reset_reuses_the_stream() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        for (image_key, window_bits, decode_key) in [
            ("zlib", ZLIB, "zlib"),
            ("raw", RAW, "raw"),
            ("gzip", GZIP, "gzip"),
            ("zlib", AUTO, "autozlib"),
            ("gzip", AUTO, "autogzip"),
        ] {
            let compressed = image(fixtures, &format!("{tag}_{image_key}"));
            let (body, used) = beam_deflate::decompress_all(window_bits, &compressed, BIG)
                .unwrap_or_else(|e| panic!("{tag}/{image_key} at {window_bits} bits: {e:?}"));
            assert_eq!(used, compressed.len(), "{tag}/{image_key}: consumed");
            assert_eq!(body, plain, "{tag}/{image_key}: the plaintext");
            assert_eq!(
                body.len(),
                number(fixtures, &format!("{tag}_{decode_key}_dec_len")),
                "{tag}/{decode_key}: the oracle's length"
            );
            assert_eq!(
                crc32(0, &body) as usize,
                number(fixtures, &format!("{tag}_{decode_key}_dec_crc")),
                "{tag}/{decode_key}: the oracle's CRC-32"
            );
        }
    }

    // `reset` reuses the handle for a second, independent stream.
    let first = image(fixtures, "r40k_zlib");
    let second = image(fixtures, "t40k_zlib");
    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    let mut buf = vec![0u8; BIG];
    assert!(!stream.at_stream_end(), "a fresh stream is not at its end");
    let step = stream.inflate(&first, &mut buf, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&buf[..step.produced], rep40k());
    assert!(stream.at_stream_end());
    stream.reset().expect("inflateReset");
    assert!(!stream.at_stream_end(), "a reset stream is not at its end");
    assert_eq!(stream.get_dictionary(), b"", "a reset forgets the window");
    assert_eq!(stream.window_bits(), ZLIB, "a reset keeps the window bits");
    let step = stream.inflate(&second, &mut buf, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&buf[..step.produced], text40k());
    assert!(stream.at_stream_end());

    // Auto-detect takes both wrappers and refuses a raw stream (there is no header).
    assert_eq!(
        beam_deflate::decompress_all(AUTO, &image(fixtures, "t40k_zlib"), BIG)
            .expect("zlib under auto")
            .0,
        text40k()
    );
    assert_eq!(
        beam_deflate::decompress_all(AUTO, &image(fixtures, "t40k_gzip"), BIG)
            .expect("gzip under auto")
            .0,
        text40k()
    );
    assert!(
        beam_deflate::decompress_all(AUTO, &image(fixtures, "t40k_raw"), BIG).is_err(),
        "auto-detect has no header to detect in a raw stream"
    );

    // The small corpus, compressed by a second oracle boot and decoded here.
    let corpus = zsupport::corpus_small();
    let mut cases = Vec::new();
    for (_, data) in &corpus {
        cases.push(Case::zlib(6, data));
        cases.push(Case::raw(6, data));
        cases.push(Case::gzip(6, data));
    }
    let oracle = match oracle_batch(&cases) {
        Some(oracle) => oracle,
        None => {
            println!("SKIP inflate oracle: the corpus batch failed to run");
            return;
        }
    };
    for (index, (name, data)) in corpus.iter().enumerate() {
        for (offset, window_bits) in [(0usize, ZLIB), (1, RAW), (2, GZIP)] {
            let compressed = match &oracle[index * 3 + offset] {
                Ok(bytes) => bytes,
                Err(reason) => panic!("{name}: the oracle rejected the case: {reason}"),
            };
            let (body, used) = beam_deflate::decompress_all(window_bits, compressed, BIG)
                .unwrap_or_else(|e| panic!("{name} at {window_bits} bits: {e:?}"));
            assert_eq!(body, *data, "{name} at {window_bits} bits");
            assert_eq!(used, compressed.len(), "{name}: the whole image is one stream");
            if window_bits == RAW {
                // Auto-detect looks for a zlib or gzip *header*; a raw stream has none, so
                // it has no framing to detect. (`t40k_raw` above pins the same thing for
                // an image large enough that leading bytes could plausibly look like a
                // header; here the tiny images are rejected outright.)
                assert!(
                    beam_deflate::decompress_all(AUTO, compressed, BIG).is_err(),
                    "{name}: auto-detect cannot identify a raw stream"
                );
                continue;
            }
            assert_eq!(
                beam_deflate::decompress_all(AUTO, compressed, BIG)
                    .unwrap_or_else(|e| panic!("{name} under auto-detect: {e:?}"))
                    .0,
                *data,
                "{name}: auto-detect"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `inflateGetDictionary` after a partial decode
// ---------------------------------------------------------------------------

/// `get_dictionary` reports exactly the bytes `zlib:inflateGetDictionary/1` reports, for
/// both shapes of a partial decode.
///
/// Two details make this a real comparison rather than a tautology:
///
/// * the window is refreshed at the end of **each** `inflate()` call with only the bytes
///   that call produced, so a feed shorter than the window keeps everything while a feed
///   longer than the window keeps the last 32 KiB — both are checked;
/// * a call that ends in the zlib trailer state (`CHECK`, after the last deflate byte but
///   before the Adler-32) contributes **nothing** to the window: the C's trailer
///   bookkeeping re-bases its output counter, so `updatewindow` is handed a zero count.
///   The "no trailer" case below is exactly that shape, and the oracle agrees byte for
///   byte.
#[test]
fn the_dictionary_after_a_partial_decode_matches_the_oracle() {
    let Some(fixtures) = fixtures() else {
        return;
    };
    for (tag, plain) in bodies() {
        let compressed = image(fixtures, &format!("{tag}_zlib"));
        let half = compressed.len() / 2;

        // Shape 1: half the image. Less than 32 KiB is produced, so the dictionary is the
        // whole produced prefix.
        let mut stream = Inflate::new(ZLIB).expect("zlib 15");
        let mut buf = vec![0u8; BIG];
        let step = stream.inflate(&compressed[..half], &mut buf, Flush::None).expect("pump");
        assert_ne!(step.status, Status::StreamEnd, "{tag}: half a stream is not the end");
        let produced = &buf[..step.produced];
        assert_eq!(produced.len(), number(fixtures, &format!("{tag}_half_len")));
        assert_eq!(crc32(0, produced) as usize, number(fixtures, &format!("{tag}_half_crc")));
        let dictionary = stream.get_dictionary();
        assert_eq!(
            dictionary.len(),
            number(fixtures, &format!("{tag}_half_dict_len")),
            "{tag}: the oracle's dictionary length after half the stream"
        );
        assert_eq!(
            crc32(0, dictionary) as usize,
            number(fixtures, &format!("{tag}_half_dict_crc")),
            "{tag}: the oracle's dictionary bytes after half the stream"
        );
        assert_eq!(dictionary, produced, "{tag}: less than a window keeps everything");
        assert!(produced.len() < 32768, "{tag}: this shape stays below one window");

        // Shape 2: everything but the Adler-32 trailer, so the call ends in `CHECK` and
        // the whole body has been produced. The window is a full 32 KiB of it.
        let mut stream = Inflate::new(ZLIB).expect("zlib 15");
        let cut = compressed.len() - 4;
        let step = stream.inflate(&compressed[..cut], &mut buf, Flush::None).expect("pump");
        assert!(
            matches!(step.status, Status::Ok | Status::BufError),
            "{tag}: an unfinished stream is not an error, got {:?}",
            step.status
        );
        assert_eq!(step.produced, plain.len(), "{tag}: the whole body is decodable");
        assert_eq!(
            step.produced,
            number(fixtures, &format!("{tag}_notrailer_len")),
            "{tag}: the oracle's partial length"
        );
        assert_eq!(
            crc32(0, &buf[..step.produced]) as usize,
            number(fixtures, &format!("{tag}_notrailer_crc")),
            "{tag}: the oracle's partial CRC-32"
        );
        let dictionary = stream.get_dictionary();
        assert_eq!(
            dictionary.len(),
            number(fixtures, &format!("{tag}_notrailer_dict_len")),
            "{tag}: the oracle's dictionary length"
        );
        assert_eq!(
            crc32(0, dictionary) as usize,
            number(fixtures, &format!("{tag}_notrailer_dict_crc")),
            "{tag}: the oracle's dictionary bytes"
        );
        assert_eq!(dictionary, &plain[plain.len() - 32768..], "{tag}: the last window");
    }
}
