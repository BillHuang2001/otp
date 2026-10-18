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
//! (`V4_PORT_EXT`, 120), so both cases `agree` on their stable leading tag byte. A **local**
//! reference round-trips too: `make_ref/0` encodes to `NEWER_REFERENCE_EXT` (90) and decodes
//! back, and both a pid and a reference come back **identical** to what went in, so those are
//! pinned by identity/shape
//! (`binary_to_term(term_to_binary(self())) =:= self()`,
//! `is_reference(binary_to_term(term_to_binary(make_ref())))`) — the per-VM-start id/creation
//! bytes of a pid/ref/port image are still never asserted.
//!
//! ## Large maps (`MAP_EXT` / HAMT)
//! A map with **more than 32 keys** builds, encodes and decodes in both runtimes:
//! `maps:from_list([{N,N} || N <- lists:seq(1,33)])` round trips to a 33-entry map in each
//! (a 33-pair `MAP_EXT`), so the >32-key HAMT image carries no divergence. (The 32-key edge
//! itself is still a flatmap in both.)
//!
//! ## `compressed` images: the round trip is pinned, the zlib payload is **not**
//! `term_to_binary(T, [compressed])` wraps the term in `VERSION_MAGIC` + `COMPRESSED` (80) +
//! a 32-bit big-endian uncompressed size + a zlib stream. The oracle and the Rust runtime
//! agree on the framing (the leading `<<131,80,…>>` and the size field) **and** on the
//! trailing Adler-32 checksum, but their zlib *middle* differs byte-for-byte (different
//! compressor output), so no case pins the compressed payload — only the `binary_to_term`
//! round trip is asserted. Exposing only the first two bytes (`<<131,80>>`) stays safe and is
//! pinned in `super::wire_extra`'s `co_prefix_two_value`.
//!
//! ## The `term_to_binary/2` option matrix
//! The last block pins the option *shapes* the encoding is driven through, each as the oracle's
//! own stable line: `[compressed]` on a body below the compressor's threshold is a no-op, so its
//! image is **byte-identical** to the plain one (a small tuple and the two-byte empty list), and
//! a compressible body's COMPRESSED (80) header carries a 32-bit big-endian uncompressed-size
//! field (pinned as the raw 4-byte field, never the zlib middle); the bare atom option
//! `minor_version` (no `{minor_version, N}` value) is `badarg` in both runtimes; `{minor_version,1}`
//! renders an atom as the legacy `ATOM_EXT` (100) inside a compound term; and `{minor_version,2}`
//! keeps `BIT_BINARY_EXT` (77) for a non-byte-aligned bitstring.
//!
//! ## The legacy float dialect: `{minor_version,0}` emits `FLOAT_EXT` (99), byte-exact
//! Version `0` is the one minor version that changes the **float** tag: `term_to_binary(F,
//! [{minor_version,0}])` emits the pre-OTP-20 `FLOAT_EXT` (99), whose body is the C
//! `printf("%.20e")` text of the double NUL-padded to 31 bytes —
//! `term_to_binary(1.5, [{minor_version,0}])` is `<<131,99,"1.50000000000000000000e+00",
//! 0,0,0,0,0>>` (33 bytes: `VERSION_MAGIC` + the tag + the 26-character text + five NULs).
//! Because that body is the encoder's canonical *text* rendering and not a floating-point
//! payload, the whole image is byte-stable, so it is pinned byte-for-byte —
//! both standalone and nested in a compound term (where the atom beside it keeps the
//! default `SMALL_ATOM_UTF8_EXT` (119), since version `1` is what selects the legacy atom
//! tag). Versions 1 and 2 both emit `NEW_FLOAT_EXT` (70).
//!
//! ## The `local` option: the node hash is masked, the rest of the image is pinned
//! `term_to_binary(Term, [local])` wraps the image in `VERSION_MAGIC` (131) + `LOCAL_EXT` (121)
//! followed by a **4-byte** payload — the node's `local_node_hash` — and then the ordinary term
//! body **without** its own version byte: `term_to_binary(1, [local])` is
//! `<<131,121,H,H,H,H,97,1>>` (8 bytes). The hash is derived per **VM start** (seeded from the
//! hostname/pid/time-offset and the monotonic clock), so on *both* runtimes two runs of the same
//! expression print different bytes there — a raw byte golden would drift the oracle's own output.
//! `etf/term_to_binary_local_masks_the_node_hash` therefore masks exactly those four bytes (and
//! reports the image's `byte_size/1`, which is hash-independent) and compares the remaining image
//! — the `LOCAL_EXT` prologue plus the whole body, captured through `binary_to_term` as well as
//! byte-for-byte — against the oracle's. The per-VM hash bytes themselves are never asserted.
//! The same rule marks an **internal id's node atom**: `term_to_binary(self(), [local])` carries
//! `NIL_EXT` (106) in the `NEW_PID_EXT`'s node position (the local node, spelled without its
//! name), so its image length is node-name-independent — pinned, with the hash masked, by
//! `etf/term_to_binary_local_replaces_the_local_node_atom`, which also pins that the plain and
//! the `[safe]` `binary_to_term/2` both decode the image back to the very same pid.
//! (A **foreign** hash — a `LOCAL_EXT` image whose four payload bytes are not this VM's — is
//! deliberately *not* a case: the oracle rejects it and this runtime decodes it, so pinning it
//! would be a mismatch-fixing exercise, not an A.2 encoding claim.)

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
        // -- the local pid/reference identity round trip (A.2/A.5) --------------------------
        //
        // A `NEW_PID_EXT`/`NEWER_REFERENCE_EXT` image carries the node's `(name, creation)`
        // identity, so its bytes are per-VM-start and never asserted: what is pinned is that the
        // decoded value **is** the value that went in (`=:=` for the pid, the `is_reference/1`
        // shape for the reference — a ref's raw bytes are VM-unique). The compressed variant
        // carries a pid and a reference past the 64-byte on-heap limit, and then observes them
        // only through stable booleans/integers.
        Case {
            id: "etf/roundtrip_self_pid_identity",
            description: "binary_to_term(term_to_binary(self())) is the very pid that called it",
            contracts: &[Contract::BeamAsm, Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(self())) =:= self()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_reference_shape",
            description: "term_to_binary/1 of make_ref/0 encodes and decodes back to a reference",
            contracts: &[Contract::BeamAsm, Contract::Distribution],
            kind: CaseKind::Eval {
                // The reference itself is per-VM-start, so only its *type* is observed.
                expr: "is_reference(binary_to_term(term_to_binary(make_ref())))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_compressed_stable_term",
            description: "a `[compressed]` image >64 bytes round-trips through binary_to_term/1",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,lists:seq(1,64)}, [compressed])) =:= {a,lists:seq(1,64)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/roundtrip_compressed_pid_ref_payload",
            description: "a `[compressed]` image past the on-heap limit brings back the same pid and reference",
            contracts: &[Contract::BeamAsm, Contract::Distribution],
            kind: CaseKind::Eval {
                // The pid/reference are compared *to the values that went in*, so the golden is a
                // stable `{true,true,64}` regardless of the per-VM-start ids.
                expr: "begin P = self(), Ref = make_ref(), {P2,Ref2,Tail} = binary_to_term(term_to_binary({P,Ref,lists:seq(1,64)}, [compressed])), {P2 =:= P, Ref2 =:= Ref, length(Tail)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,64}"),
                exit_code: 0,
            },
        },
        // -- the big-integer (`SMALL_BIG_EXT`) images and the decoder's normalization -------
        //
        // An integer outside the 60-bit immediate / 64-bit signed range encodes as
        // `SMALL_BIG_EXT` (110): `[131][110][sign:u8][byte count:u8][magnitude, LITTLE-endian]`.
        // `binary_to_term/1` normalizes the *value*, not the tag, so a `SMALL_BIG_EXT` blob whose
        // magnitude fits a machine word comes back out re-encoded as `SMALL_INT_EXT`/`INT_EXT`.
        Case {
            id: "etf/term_to_binary_bignum_positive",
            description: "an integer above 2^64-1 encodes as SMALL_BIG_EXT (110) with a little-endian magnitude",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "123456789012345678901234567890",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,110,13,0,210,10,63,78,238,224,115,195,246,15,233,142,1>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bignum_negative",
            description: "a negative SMALL_BIG_EXT image carries the sign byte 1 and the same magnitude",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "0-123456789012345678901234567890",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,110,13,1,210,10,63,78,238,224,115,195,246,15,233,142,1>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_int64_max_small_big",
            description: "2^63-1 overflows the 60-bit immediate and encodes as an 8-byte SMALL_BIG_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "9223372036854775807",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,8,0,255,255,255,255,255,255,255,127>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_small_big_u64_overflow",
            description: "2^64 needs a ninth magnitude byte, so the SMALL_BIG_EXT length field grows to 9",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "18446744073709551616",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,9,0,0,0,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_small_big_normalization",
            description: "decoding a SMALL_BIG_EXT blob re-encodes the value by its magnitude (small int / int / bignum)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // `<<131,110,2,0,1,0>>` is the two-byte bignum 1 => SMALL_INT_EXT 1;
                // `<<131,110,2,0,255,255>>` is 65535 => INT_EXT; the 9-byte negative blob stays a
                // bignum. The probe renders `term_to_binary/1` of the *decoded* tuple, so the
                // canonical re-encoding is what the golden pins.
                expr: "{binary_to_term(<<131,110,2,0,1,0>>), binary_to_term(<<131,110,2,0,255,255>>), \
                       binary_to_term(<<131,110,9,1,0,0,0,0,0,0,0,0,1>>)}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,97,1,98,0,0,255,255,110,9,1,0,0,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        // -- the string-tag normalization and the empty-atom forms -------------------------
        //
        // Three string-ish input tags decode to the same term, and the encoder canonicalizes on
        // the way out: an empty `STRING_EXT` becomes `NIL_EXT` (106), and a `LIST_EXT` whose
        // elements are all bytes becomes `STRING_EXT` (107).
        Case {
            id: "etf/binary_to_term_string_normalization",
            description: "an empty STRING_EXT decodes to [] and a byte-listed LIST_EXT decodes to a string",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{binary_to_term(<<131,107,0,0>>), \
                       binary_to_term(<<131,108,0,0,0,2,97,1,97,2,106>>)}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,106,107,0,2,1,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_empty_atom_forms",
            description: "the three atom tags decode to the empty atom, whatever their name encoding was",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // Tag 100 `ATOM_EXT` (Latin-1), 118 `ATOM_UTF8_EXT`, 115 `SMALL_ATOM_EXT`; all
                // three with a zero-length name decode to `''`, which re-encodes as 119.
                expr: "{binary_to_term(<<131,100,0,0>>), binary_to_term(<<131,118,0,0>>), \
                       binary_to_term(<<131,115,0>>)}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,0,119,0,119,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_string_ext_u16_length_boundary",
            description: "a 256-byte byte list fills STRING_EXT's 16-bit length to its first overflow value",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // `STRING_EXT` (107) carries a big-endian 16-bit length; 256 is the smallest value
                // that needs its high byte. The image is 1 + 1 + 2 + 256 = 260 bytes.
                expr: "begin B = term_to_binary(lists:seq(0,255)), \
                       {byte_size(B), binary:part(B,0,4)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {260,<<131,107,1,0>>}"),
                exit_code: 0,
            },
        },
        // -- the `{minor_version,1}` dialect and the `[used]` option ----------------------
        Case {
            id: "etf/term_to_binary_minor_version_1_dialect",
            description: "term_to_binary/2 with {minor_version,1} wraps a legacy ATOM_EXT (100) image in a binary",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // Where the default (version-2) encoder emits `SMALL_ATOM_UTF8_EXT` (119), the
                // version-1 dialect emits `ATOM_EXT` (100) with a 16-bit length.
                expr: "{term_to_binary(hello, [{minor_version, 1}]), \
                       term_to_binary({ok,42}, [{minor_version, 1}])}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,104,2,109,0,0,0,9,131,100,0,5,104,101,108,108,111,\
                    109,0,0,0,10,131,104,2,100,0,2,111,107,97,42>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_used_option",
            description: "binary_to_term/2 with [used] answers {Term, BytesUsed} on an uncompressed image",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // An uncompressed image is consumed exactly, so `used` is the whole byte count
                // (9 here) in both runtimes.
                expr: "{binary_to_term(term_to_binary({ok,42}), [used]), \
                       binary_to_term(term_to_binary(lists:seq(1,5)), [used])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{{ok,42},9},{[1,2,3,4,5],9}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_nested_mixed_roundtrip",
            description: "a tuple mixing an atom, a string list, an integer-keyed map and a binary round-trips",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,[1,2],#{1=>2},<<3>>}))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,4,119,1,97,107,0,2,1,2,116,0,0,0,1,97,1,97,2,109,0,0,0,1,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_bit_binary_nonzero_padding_is_badarg",
            description: "a BIT_BINARY_EXT whose padding byte has non-zero low bits is rejected with badarg",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                // Tag 77 with length 2 bits: only the top two bits of the single payload byte are
                // meaningful, so a payload of 255 (three set padding bits) is a malformed image.
                expr: "binary_to_term(<<131,77,0,0,0,2,1,255>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "etf/roundtrip_improper_list",
            description: "an improper list round-trips through term_to_binary/1 as LIST_EXT with a tail",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([a|b]))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,1,119,1,97,119,1,98>>"),
                exit_code: 0,
            },
        },
        // -- the `term_to_binary/2` option matrix: `compressed` on a small body and the
        // `minor_version` dialect: every golden below is the oracle's own line --------
        Case {
            id: "etf/term_to_binary_compressed_small_tuple_is_verbatim",
            description: "the `compressed` option leaves a small tuple uncompressed (no COMPRESSED wrapper is emitted)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary({1,2}, [compressed])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,7,131,104,2,97,1,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_compressed_nil_is_verbatim",
            description: "the `compressed` option leaves the two-byte empty list uncompressed",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary([], [compressed])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,2,131,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_compressed_declared_size_field",
            description: "the COMPRESSED (80) header's 32-bit big-endian uncompressed-size field for a compressible 3000-byte string body",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary:part(term_to_binary(lists:duplicate(3000,$a), [compressed]), 2, 4)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,4,0,0,11,187>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_bare_minor_version_is_badarg",
            description: "the bare atom option `minor_version` (no `{minor_version, N}` value) is badarg",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [minor_version])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "etf/term_to_binary_minor_version_1_tuple_legacy_atom",
            description: "a {minor_version,1} compound term encodes its atom as the legacy ATOM_EXT (100)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary({hello, 1}, [{minor_version, 1}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,13,131,104,2,100,0,5,104,101,108,108,111,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_minor_version_2_bit_binary_tag",
            description: "a {minor_version,2} non-byte-aligned bitstring keeps BIT_BINARY_EXT (77) with its unused-bit count",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<5:3>>, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,8,131,77,0,0,0,1,3,160>>"),
                exit_code: 0,
            },
        },
        // -- `{minor_version,0}`: the legacy ASCII FLOAT_EXT (99) dialect, byte-exact -------
        // `{minor_version,0}` encodes a float as the pre-OTP-20 `FLOAT_EXT` (99): a body of
        // the C `printf("%.20e")` text NUL-padded to 31 bytes — the 26-character
        // `1.50000000000000000000e+00` plus five NULs, so the whole image is 33 bytes
        // (`131,99` + the body). The body is the encoder's canonical rendering, not a
        // floating-point payload, so it is byte-stable — and a float nested in a compound
        // term uses the same dialect (the tuple case also shows that the *atom* beside it
        // stays the default `SMALL_ATOM_UTF8_EXT` (119), since version 1 is what selects the
        // legacy atom tag).
        Case {
            id: "etf/term_to_binary_minor_version_0_float_ext_bytes",
            description: "a {minor_version,0} float is the legacy ASCII FLOAT_EXT (99): a \
                          %.20e text body NUL-padded to 31 bytes (a 33-byte image)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,\
                     48,101,43,48,48,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_minor_version_0_nested_legacy_float",
            description: "a {minor_version,0} compound term nests the legacy FLOAT_EXT (99) \
                          float while its atom keeps the default SMALL_ATOM_UTF8_EXT (119)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "term_to_binary({1.5, hello}, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,104,2,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,\
                     48,48,48,101,43,48,48,0,0,0,0,0,100,0,5,104,101,108,108,111>>",
                ),
                exit_code: 0,
            },
        },
        // -- the `local` option: the 4-byte node hash is per-VM-start, so it is masked and the
        // rest of the LOCAL_EXT image is pinned byte-for-byte (see the module docs) --------
        Case {
            id: "etf/term_to_binary_local_masks_the_node_hash",
            description: "term_to_binary/2 with `local` emits VERSION_MAGIC + LOCAL_EXT (121) + a \
                          4-byte per-VM-start node hash + the body: the hash is masked, the image \
                          length and every other byte (and the decode round trip) are pinned for \
                          an integer, a compound term and a 40-element list",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "begin F = fun(T) -> B = term_to_binary(T, [local]), \
                       <<Pfx:2/binary, _Hash:4/binary, Tail/binary>> = B, \
                       {byte_size(B), <<Pfx/binary, 0:32, Tail/binary>>, binary_to_term(B) =:= T} \
                       end, [F(1), F({a,\"hello\",[1,2,3]}), F(lists:seq(1,40))] end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok [{8,<<131,121,0,0,0,0,97,1>>,true},{25,<<131,121,0,0,0,0,104,3,119,1,97,\
                     107,0,5,104,101,108,108,111,107,0,3,1,2,3>>,true},{49,<<131,121,0,0,0,0,107,\
                     0,40,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,\
                     27,28,29,30,31,32,33,34,35,36,37,38,39,40>>,true}]",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_local_replaces_the_local_node_atom",
            description: "term_to_binary/2 with `local` marks an internal id's node atom as \
                          NIL_EXT (106): the image length, the masked LOCAL_EXT + NEW_PID_EXT + \
                          NIL_EXT prologue and the plain and `[safe]` decode round trips are \
                          pinned, while the node hash and the pid's own id bytes are not",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "begin B = term_to_binary(self(), [local]), \
                       <<Pfx:2/binary, _Hash:4/binary, Tail/binary>> = B, \
                       M = <<Pfx/binary, 0:32, Tail/binary>>, \
                       {byte_size(B), binary:part(M, 0, 8), binary_to_term(B) =:= self(), \
                        binary_to_term(B, [safe]) =:= self()} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {20,<<131,121,0,0,0,0,88,106>>,true,true}"),
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

    /// The local pid/reference **identity** round trips (A.2/A.5): the image bytes are
    /// per-VM-start, so the golden is the stable `~w` rendering of the identity/shape predicate.
    #[test]
    fn local_pid_and_reference_round_trips_are_pinned_by_identity() {
        let cases = group();
        // The identity/shape cases also evidence the distribution id contract (A.5); the
        // stable-term compressed round trip is pure ETF (A.2).
        for (id, contracts, stdout) in [
            (
                "etf/roundtrip_self_pid_identity",
                &[Contract::BeamAsm, Contract::Distribution][..],
                "ok true",
            ),
            (
                "etf/roundtrip_reference_shape",
                &[Contract::BeamAsm, Contract::Distribution][..],
                "ok true",
            ),
            (
                "etf/roundtrip_compressed_stable_term",
                &[Contract::BeamAsm][..],
                "ok true",
            ),
            (
                "etf/roundtrip_compressed_pid_ref_payload",
                &[Contract::BeamAsm, Contract::Distribution][..],
                "ok {true,true,64}",
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, contracts, "{id}");
            assert_eq!(case.kind_label(), "value", "{id}");
            assert_eq!(
                case.golden,
                Golden { stdout: Some(stdout), exit_code: 0 },
                "{id}"
            );
        }
    }

    /// The bignum / decoder-normalization / dialect cases added to widen A.2: every golden is the
    /// oracle's own single line (re-measured with `differential run <id>`), and the one malformed
    /// image pins the decoder's `badarg` refusal with the matching exit code.
    #[test]
    fn the_small_big_and_dialect_cases_pin_the_oracle_lines() {
        let cases = group();
        for (id, label, stdout, exit_code) in [
            (
                "etf/term_to_binary_bignum_positive",
                "term_to_binary",
                "ok <<131,110,13,0,210,10,63,78,238,224,115,195,246,15,233,142,1>>",
                0,
            ),
            (
                "etf/term_to_binary_bignum_negative",
                "term_to_binary",
                "ok <<131,110,13,1,210,10,63,78,238,224,115,195,246,15,233,142,1>>",
                0,
            ),
            (
                "etf/term_to_binary_int64_max_small_big",
                "term_to_binary",
                "ok <<131,110,8,0,255,255,255,255,255,255,255,127>>",
                0,
            ),
            (
                "etf/term_to_binary_small_big_u64_overflow",
                "term_to_binary",
                "ok <<131,110,9,0,0,0,0,0,0,0,0,0,1>>",
                0,
            ),
            (
                "etf/binary_to_term_small_big_normalization",
                "term_to_binary",
                "ok <<131,104,3,97,1,98,0,0,255,255,110,9,1,0,0,0,0,0,0,0,0,1>>",
                0,
            ),
            (
                "etf/binary_to_term_string_normalization",
                "term_to_binary",
                "ok <<131,104,2,106,107,0,2,1,2>>",
                0,
            ),
            (
                "etf/binary_to_term_empty_atom_forms",
                "term_to_binary",
                "ok <<131,104,3,119,0,119,0,119,0>>",
                0,
            ),
            (
                "etf/term_to_binary_string_ext_u16_length_boundary",
                "value",
                "ok {260,<<131,107,1,0>>}",
                0,
            ),
            (
                "etf/term_to_binary_minor_version_1_dialect",
                "term_to_binary",
                "ok <<131,104,2,109,0,0,0,9,131,100,0,5,104,101,108,108,111,\
                 109,0,0,0,10,131,104,2,100,0,2,111,107,97,42>>",
                0,
            ),
            (
                "etf/binary_to_term_used_option",
                "value",
                "ok {{{ok,42},9},{[1,2,3,4,5],9}}",
                0,
            ),
            (
                "etf/binary_to_term_nested_mixed_roundtrip",
                "term_to_binary",
                "ok <<131,104,4,119,1,97,107,0,2,1,2,116,0,0,0,1,97,1,97,2,109,0,0,0,1,3>>",
                0,
            ),
            (
                "etf/binary_to_term_bit_binary_nonzero_padding_is_badarg",
                "term_to_binary",
                "exception error badarg",
                1,
            ),
            (
                "etf/roundtrip_improper_list",
                "term_to_binary",
                "ok <<131,108,0,0,0,1,119,1,97,119,1,98>>",
                0,
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), label, "{id}");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code
                },
                "{id}"
            );
        }
    }

    /// The `local` option case (A.2): the image is pinned with the four per-VM-start node-hash
    /// bytes **masked**, so the golden must (a) be a `value`-probe line, (b) name the three
    /// `LOCAL_EXT` images by their hash-independent `byte_size/1`, and (c) carry no unique node
    /// hash — only the zeroed stand-in at every `LOCAL_EXT` position.
    #[test]
    fn the_local_option_case_masks_the_node_hash_and_pins_the_rest() {
        let cases = group();
        let case = cases
            .iter()
            .find(|c| c.id == "etf/term_to_binary_local_masks_the_node_hash")
            .expect("registered");
        assert_eq!(case.contracts, &[Contract::BeamAsm]);
        assert_eq!(case.kind_label(), "value");
        assert_eq!(case.golden.exit_code, 0);

        let expr = case.expr().expect("an expression case");
        // The mask itself: skip VERSION_MAGIC + LOCAL_EXT (2 bytes), drop exactly the 4 hash
        // bytes, and splice a zeroed 32-bit field back in their place.
        assert!(
            expr.contains(
                "<<Pfx:2/binary, _Hash:4/binary, Tail/binary>> = B, \
                 {byte_size(B), <<Pfx/binary, 0:32, Tail/binary>>"
            ),
            "the expression masks exactly the 4 node-hash bytes"
        );
        // The three bodies cover a small int, a compound term (atom + string + list) and a
        // >32-element list, so the inner image length varies.
        for needle in ["F(1)", "F({a,\"hello\",[1,2,3]})", "F(lists:seq(1,40))"] {
            assert!(expr.contains(needle), "the expression covers {needle}");
        }
        // The round trip is asserted inside the expression (the real image decodes back).
        assert!(expr.contains("binary_to_term(B) =:= T"));
        // The expression must not bind a variable the probe wrapper binds (`V`, `C`, `R`).
        assert!(
            !expr.contains("R/") && !expr.contains("V =") && !expr.contains("C ="),
            "the expression must not shadow the probe wrapper's own bindings"
        );

        let golden = case.golden.stdout.expect("golden");
        assert!(golden.starts_with("ok ["), "{golden}");
        // Every LOCAL_EXT position carries the zeroed stand-in `<<131,121,0,0,0,0,`.
        let local = "<<131,121,0,0,0,0,";
        assert_eq!(
            golden.matches(local).count(),
            3,
            "one masked LOCAL_EXT image per covered term: {golden}"
        );
        // …and no other LOCAL_EXT prologue (i.e. an unmasked, unique hash) appears.
        assert_eq!(golden.matches("<<131,121,").count(), 3, "{golden}");
        for size in ["{8,", "{25,", "{49,"] {
            assert!(golden.contains(size), "the image length {size} is pinned: {golden}");
        }
    }

    /// The `{minor_version,0}` legacy float dialect (A.2): the ASCII `FLOAT_EXT` (99) image is
    /// the encoder's canonical `%.20e` text NUL-padded to 31 bytes, so it is byte-stable and
    /// pinned exactly — standalone and nested. Grow this test with the group.
    #[test]
    fn the_minor_version_zero_float_dialect_is_pinned_byte_exactly() {
        let cases = group();
        for (id, stdout) in [
            (
                "etf/term_to_binary_minor_version_0_float_ext_bytes",
                "ok <<131,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,\
                 48,101,43,48,48,0,0,0,0,0>>",
            ),
            (
                "etf/term_to_binary_minor_version_0_nested_legacy_float",
                "ok <<131,104,2,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,\
                 48,48,48,101,43,48,48,0,0,0,0,0,100,0,5,104,101,108,108,111>>",
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), "value", "{id}");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code: 0
                },
                "{id}"
            );
            // The image must be the legacy `FLOAT_EXT` (99) tag, never `NEW_FLOAT_EXT` (70).
            let expr = case.expr().expect("an expression case");
            assert!(
                expr.contains("[{minor_version, 0}]"),
                "case {id} must drive the version-0 encoder: {expr}"
            );
            assert!(stdout.contains(",99,"), "case {id} must carry the FLOAT_EXT tag: {stdout}");
            assert!(
                !stdout.contains(",70,"),
                "case {id} must not carry a NEW_FLOAT_EXT tag: {stdout}"
            );
        }
        // The legacy body ends in the five NUL padding bytes of the fixed 31-byte body.
        for id in [
            "etf/term_to_binary_minor_version_0_float_ext_bytes",
            "etf/term_to_binary_minor_version_0_nested_legacy_float",
        ] {
            let golden = cases
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.golden.stdout)
                .expect("registered");
            assert!(
                golden.contains("101,43,48,48,0,0,0,0,0"),
                "case {id} must pin the `e+00` tail and its NUL padding: {golden}"
            );
        }
    }

    /// The `local`-marked internal id case (A.2): the node atom of a local pid is `NIL_EXT`, so
    /// the golden pins the masked prologue and the node-name-independent image length, and never
    /// the pid's own id bytes.
    #[test]
    fn the_local_option_marks_a_local_pids_node_atom_as_nil_ext() {
        let cases = group();
        let case = cases
            .iter()
            .find(|c| c.id == "etf/term_to_binary_local_replaces_the_local_node_atom")
            .expect("registered");
        assert_eq!(case.contracts, &[Contract::BeamAsm]);
        assert_eq!(case.kind_label(), "value");

        let expr = case.expr().expect("an expression case");
        for needle in [
            "term_to_binary(self(), [local])",
            "<<Pfx:2/binary, _Hash:4/binary, Tail/binary>> = B",
            "<<Pfx/binary, 0:32, Tail/binary>>",
            "binary:part(M, 0, 8)",
            "binary_to_term(B) =:= self()",
            "binary_to_term(B, [safe]) =:= self()",
        ] {
            assert!(expr.contains(needle), "the expression needs {needle:?}");
        }
        // The observable is the masked prologue, the length and the two round trips — never the
        // pid's per-VM-start id/serial/creation bytes and never the node hash.
        assert_eq!(
            case.golden,
            Golden {
                stdout: Some("ok {20,<<131,121,0,0,0,0,88,106>>,true,true}"),
                exit_code: 0,
            }
        );
    }
}
