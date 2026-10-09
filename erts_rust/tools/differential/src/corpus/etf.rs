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

//! Contract **A.2** — `.beam` binary and External Term Format bytes.
//!
//! These cases pin the *canonical* encoding a runtime must produce: `term_to_binary/1`
//! output is compared byte-for-byte by the harness, so an alternative but legal encoder
//! is not enough — the bytes must match the reference runtime exactly.
//!
//! The group also covers the External Term Format **encode** surface (the tags
//! `beam_codec` emits). Two of those tags embed a per-VM-start value, so their image is
//! **never** pinned byte-for-byte (a byte golden would drift the oracle's own output on the
//! next VM start); only the stable leading **tag byte** is asserted:
//!
//! * a pid ⇒ `NEW_PID_EXT` (88),
//! * an internal port ⇒ `V4_PORT_EXT` (120).
//!
//! The Rust runtime encodes a local pid (`NEW_PID_EXT`, 88) and an internal port
//! (`V4_PORT_EXT`, 120), so both cases `agree` on their stable leading tag byte. Encoding an
//! external **reference** is the runtime's `beam_rt: unsupported: external_term` capability
//! gap, so no ref case is registered here.
//!
//! ## Known divergence (handoff — deliberately no case registered)
//! A map with **more than 32 keys** (the large-map / hashmap image) cannot be pinned today:
//! the Rust runtime cannot even *build* such a map — `maps:from_list/1` and repeated
//! `maps:put/3` both raise `error:system_limit` once a map grows past 32 entries, where the
//! oracle builds and encodes it. Observed for `maps:from_list([{N,N} || N <- lists:seq(1,33)])`:
//! oracle ⇒ `ok <<131,116,0,0,0,33,…>>` (a 33-pair `MAP_EXT`), Rust ⇒ `exception error system_limit`.
//! `system_limit` is a plain Erlang reason, not the runtime's documented `unsupported`
//! convention, so registering the case would introduce a real **mismatch** — hence it is left
//! out until the runtime's large-map representation lands. (The 32-key edge itself, still a
//! flatmap, does `agree`.)
//!
//! ## `compressed` images: the round trip is pinned, the zlib payload is **not**
//! `term_to_binary(T, [compressed])` wraps the term in `VERSION_MAGIC` + `COMPRESSED` (80) +
//! a 32-bit big-endian uncompressed size + a zlib stream. The oracle and the Rust runtime
//! agree on the framing (the leading `<<131,80,…>>` and the size field) **and** on the
//! trailing Adler-32 checksum, but their zlib *middle* differs byte-for-byte (different
//! compressor output), so no case pins the compressed payload — only the `binary_to_term`
//! round trip is asserted. Exposing only the first two bytes (`<<131,80>>`) stays safe and is
//! pinned in `super::wire_extra`'s `co_prefix_two_value`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.2 cases: canonical ETF encodings plus a `binary_to_term/1` round-trip.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "etf/term_to_binary_tuple",
            description: "term_to_binary/1 yields the canonical ETF bytes of a 2-tuple {ok,42}",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{ok,42}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,2,111,107,97,42>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom",
            description: "an atom encodes as SMALL_ATOM_UTF8_EXT with its byte length",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "hello",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_map",
            description: "a small map encodes as MAP_EXT with a 32-bit pair count",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "#{a => 1}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a mixed term",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({ok,[1,2,3],<<\"x\">>}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[1,2,3],<<120>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_list",
            description: "the empty list encodes as NIL_EXT (106)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_tuple",
            description: "the empty tuple encodes as SMALL_TUPLE_EXT with arity 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_binary",
            description: "the empty binary encodes as BINARY_EXT with length 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_map",
            description: "the empty map encodes as MAP_EXT with a 32-bit pair count of 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "#{}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_float",
            description: "a float encodes as NEW_FLOAT_EXT (IEEE-754 big-endian)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "3.14",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,70,64,9,30,184,81,235,133,31>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_small_int_255",
            description: "the largest SMALL_INT_EXT value (255) is a single unsigned byte",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "255",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,255>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_int32_256",
            description: "256 overflows SMALL_INT_EXT and encodes as a 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "256",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,0,0,1,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_negative_int",
            description: "a negative integer encodes as a signed 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "0-8",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,255,255,255,248>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_string",
            description: "a printable character list encodes as STRING_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "\"ab\"",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,2,97,98>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_list_nonstring",
            description: "a list with a non-byte element encodes as LIST_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[1,2,300]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,3,97,1,97,2,98,0,0,1,44,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_small_int",
            description: "binary_to_term/1 decodes a SMALL_INT_EXT blob",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_negative_int",
            description: "binary_to_term/1 inverts term_to_binary/1 for a negative integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(0-8))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -8"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_zero",
            description: "integer 0 encodes as SMALL_INT_EXT with a zero byte",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "0",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_int65535",
            description: "65535 is the largest 16-bit value, encoded as a 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "65535",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,0,0,255,255>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_int65536",
            description: "65536 overflows 16 bits and still encodes as a 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "65536",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,0,1,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_true",
            description: "the atom true encodes as SMALL_ATOM_UTF8_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "true",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_false",
            description: "the atom false encodes as SMALL_ATOM_UTF8_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "false",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,102,97,108,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_map",
            description: "binary_to_term/1 inverts term_to_binary/1 for a single-key map",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(#{a => 1}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok #{a => 1}"),
                exit_code: 0,
            },
        },
        // -- the `{minor_version, _}` encode dialect / funs / integer-keyed maps -------
        Case {
            id: "etf/term_to_binary_minor_version_2_atom",
            description: "term_to_binary/2 with {minor_version,2} keeps SMALL_ATOM_UTF8_EXT (119) for an atom",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,8,131,119,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_minor_version_2_float",
            description: "term_to_binary/2 with {minor_version,2} encodes a float as NEW_FLOAT_EXT (70)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,10,131,70,63,248,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_export_fun",
            description: "an exported function fun encodes as EXPORT_EXT (113): module, function, arity",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun lists:reverse/1)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,20,131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_integer_key_map_order",
            description: "a small integer-keyed map encodes as MAP_EXT with its keys in ascending order",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{1 => a, 2 => b, 3 => c})",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,21,131,116,0,0,0,3,97,1,119,1,97,97,2,119,1,98,97,3,119,1,99>>"),
                exit_code: 0,
            },
        },
        // -- encode surface: pids, ports, bitstrings, binaries, UTF-8 atoms -----------------
        Case {
            id: "etf/term_to_binary_new_pid_ext",
            description: "term_to_binary/1 of a pid emits NEW_PID_EXT (88) as its leading tag byte",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary:at(term_to_binary(self()),1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 88"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_v4_port_ext",
            description: "term_to_binary/1 of an internal port emits V4_PORT_EXT (120) as its leading tag byte",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"cat\"},[binary]), Tag = binary:at(term_to_binary(P),1), port_close(P), Tag end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 120"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bit_binary",
            description: "a non-byte-aligned bitstring encodes as BIT_BINARY_EXT (77) with its unused-bit count",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<1:1>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,77,0,0,0,1,1,128>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_short_binary",
            description: "a short binary encodes as BINARY_EXT (109) with a 32-bit length (no small-binary tag exists)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<\"hi\">>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,2,104,105>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_binary_100_bytes",
            description: "a 100-byte binary encodes as BINARY_EXT (109) with a 32-bit length of 100",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "list_to_binary(lists:duplicate(100,$a))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,100,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97,97>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_utf8_latin1",
            description: "a non-ASCII Latin-1-range atom (U+00E9) encodes as SMALL_ATOM_UTF8_EXT, not legacy ATOM_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "'\\x{e9}'",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,195,169>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_utf8_bmp",
            description: "a BMP non-Latin-1 atom (U+20AC) encodes as SMALL_ATOM_UTF8_EXT with 3 UTF-8 bytes",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "'\\x{20ac}'",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,3,226,130,172>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_utf8_astral",
            description: "an astral non-BMP atom (U+1F600) encodes as SMALL_ATOM_UTF8_EXT with 4 UTF-8 bytes",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "'\\x{1f600}'",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,240,159,152,128>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom_utf8_mixed",
            description: "a mixed ASCII + non-ASCII atom encodes as SMALL_ATOM_UTF8_EXT with the full UTF-8 byte length",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "'hello\\x{e9}world'",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,12,104,101,108,108,111,195,169,119,111,114,108,100>>"),
                exit_code: 0,
            },
        },
        // -- edge cases: length boundaries, bitstrings, compression, framing, deep nesting -----
        Case {
            id: "etf/binary_ext_255_and_256_shape",
            description: "a 255- and a 256-byte binary both stay BINARY_EXT (109), a six-byte header plus the payload",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[{binary:at(term_to_binary(binary:copy(<<0>>,N)),1), byte_size(term_to_binary(binary:copy(<<0>>,N)))} || N <- [255,256]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{109,261},{109,262}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_empty_binary",
            description: "binary_to_term/1 inverts term_to_binary/1 for the empty binary",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(<<>>)) =:= <<>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bit_binary_3_bits",
            description: "a 3-bit bitstring encodes as BIT_BINARY_EXT (77) with a bit count of 3 and a 5-bit trailing pad",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<5:3>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,77,0,0,0,1,3,160>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bit_binary_9_bits",
            description: "a 9-bit bitstring spans two bytes with a bit count of 1 (a 7-bit trailing pad)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<1:9>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,77,0,0,0,2,1,0,128>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bit_binary_byte_aligned",
            description: "a byte-aligned bitstring collapses to BINARY_EXT (109): <<1:8>> is a 7-byte image, <<1:16>> an 8-byte one",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{[binary:at(term_to_binary(<<1:8>>),1), byte_size(term_to_binary(<<1:8>>))], [binary:at(term_to_binary(<<1:16>>),1), byte_size(term_to_binary(<<1:16>>))]}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[109,7],[109,8]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_compressed",
            description: "a `[compressed]` image round-trips (the zlib payload is never byte-compared)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,lists:seq(1,200)}, [compressed])) =:= {a,lists:seq(1,200)}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_compressed_level9",
            description: "a `{compressed,9}` image round-trips",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(lists:seq(1,500), [{compressed,9}])) =:= lists:seq(1,500)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_nil_blob",
            description: "binary_to_term/1 decodes a hand-built VERSION_MAGIC + NIL_EXT (106) blob",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_tuple_blob",
            description: "binary_to_term/1 decodes a hand-built VERSION_MAGIC + SMALL_TUPLE_EXT (104) blob",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,97,1,97,2>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_minor_version_1",
            description: "a `{minor_version,1}` image round-trips",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,1}, [{minor_version,1}])) =:= {a,1}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_string_255_and_256",
            description: "a 255- and a 256-byte character list both encode as STRING_EXT (107)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[binary:at(term_to_binary(lists:duplicate(N,$a)),1) || N <- [255,256]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [107,107]"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_list_255_and_256",
            description: "a 255- and a 256-element non-byte list both encode as LIST_EXT (108)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[binary:at(term_to_binary(lists:duplicate(N,300)),1) || N <- [255,256]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [108,108]"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_deep_nested_list",
            description: "a deeply nested list round-trips",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,[2,[3,[4,[5]]]]])) =:= [1,[2,[3,[4,[5]]]]]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_large_integer_list",
            description: "a 500-element integer list round-trips",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(lists:seq(1,500))) =:= lists:seq(1,500)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_mixed_terms",
            description: "a spread of mixed-type terms each round-trips through term_to_binary/1",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(E) -> binary_to_term(term_to_binary(E)) =:= E end, [[], [1,2,3], <<1,2>>, <<5:3>>, {a,b}, #{k=>v}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, Golden};
    use crate::corpus::Contract;

    #[test]
    fn new_etf_cases_pin_canonical_term_to_binary_bytes() {
        let cases = group();
        for id in [
            "etf/term_to_binary_minor_version_2_atom",
            "etf/term_to_binary_minor_version_2_float",
            "etf/term_to_binary_export_fun",
            "etf/term_to_binary_integer_key_map_order",
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), "term_to_binary", "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
            assert!(case.golden.stdout.is_some_and(|s| !s.is_empty()), "{id}");
        }
    }

    #[test]
    fn encode_surface_cases_pin_bytes_and_stable_shapes() {
        let cases = group();

        // Byte-image cases (default `Probe::TermToBinary`): the exact canonical encoding.
        for (id, stdout) in [
            ("etf/term_to_binary_bit_binary", "ok <<131,77,0,0,0,1,1,128>>"),
            ("etf/term_to_binary_short_binary", "ok <<131,109,0,0,0,2,104,105>>"),
            ("etf/term_to_binary_atom_utf8_latin1", "ok <<131,119,2,195,169>>"),
            ("etf/term_to_binary_atom_utf8_bmp", "ok <<131,119,3,226,130,172>>"),
            ("etf/term_to_binary_atom_utf8_astral", "ok <<131,119,4,240,159,152,128>>"),
            (
                "etf/term_to_binary_atom_utf8_mixed",
                "ok <<131,119,12,104,101,108,108,111,195,169,119,111,114,108,100>>",
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), "term_to_binary", "{id}");
            assert_eq!(
                case.golden,
                Golden { stdout: Some(stdout), exit_code: 0 },
                "{id}"
            );
        }

        // Shape cases (`Probe::Value`): a per-VM-start pid/port image is reduced to its
        // stable leading tag byte, so the golden cannot drift the oracle's own output.
        for (id, stdout) in [
            ("etf/term_to_binary_new_pid_ext", "ok 88"),
            ("etf/term_to_binary_v4_port_ext", "ok 120"),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), "value", "{id}");
            assert_eq!(
                case.golden,
                Golden { stdout: Some(stdout), exit_code: 0 },
                "{id}"
            );
        }

        // The 100-byte binary image is BINARY_EXT (109) with a 4-byte length of 100 and
        // exactly 100 `$a` (97) bytes.
        let case = cases
            .iter()
            .find(|c| c.id == "etf/term_to_binary_binary_100_bytes")
            .expect("registered");
        let golden = case.golden.stdout.expect("golden");
        let expected = format!("ok <<131,109,0,0,0,100,{}>>", vec!["97"; 100].join(","));
        assert_eq!(golden, expected);
    }

    #[test]
    fn edge_cases_pin_boundaries_bitstrings_and_roundtrips() {
        let cases = group();
        for (id, label, stdout) in [
            (
                "etf/binary_ext_255_and_256_shape",
                "value",
                "ok [{109,261},{109,262}]",
            ),
            (
                "etf/roundtrip_empty_binary",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "etf/term_to_binary_bit_binary_3_bits",
                "term_to_binary",
                "ok <<131,77,0,0,0,1,3,160>>",
            ),
            (
                "etf/term_to_binary_bit_binary_9_bits",
                "term_to_binary",
                "ok <<131,77,0,0,0,2,1,0,128>>",
            ),
            (
                "etf/term_to_binary_bit_binary_byte_aligned",
                "value",
                "ok {[109,7],[109,8]}",
            ),
            (
                "etf/roundtrip_compressed",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "etf/roundtrip_compressed_level9",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            ("etf/binary_to_term_nil_blob", "value", "ok []"),
            ("etf/binary_to_term_tuple_blob", "value", "ok {1,2}"),
            (
                "etf/roundtrip_minor_version_1",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "etf/term_to_binary_string_255_and_256",
                "value",
                "ok [107,107]",
            ),
            (
                "etf/term_to_binary_list_255_and_256",
                "value",
                "ok [108,108]",
            ),
            (
                "etf/roundtrip_deep_nested_list",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "etf/roundtrip_large_integer_list",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "etf/roundtrip_mixed_terms",
                "term_to_binary",
                "ok <<131,119,4,116,114,117,101>>",
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), label, "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code: 0
                },
                "{id}"
            );
        }
    }
}
