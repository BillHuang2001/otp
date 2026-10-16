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

//! Contract **A.5** — the distribution wire format (External Term Format, ETF).
//!
//! Distribution frames every message in ETF, and the handshake is a couple of `DFLAG_*`
//! control messages carrying ETF terms. That *codec* is a black box the CLI can reach:
//! an ETF blob is just bytes, so decoding a **fixed** blob that was produced by the
//! reference runtime is a genuine cross-runtime check of the wire encoding, and
//! encoding a term to the same blob is the inverse direction.
//!
//! The cases cover the concrete tags the distribution encoder/decoder actually uses:
//!
//! * numbers — `SMALL_INTEGER_EXT` (97), `INTEGER_EXT` (98), `NEW_FLOAT_EXT` (70),
//!   `SMALL_BIG_EXT` (110), `LARGE_BIG_EXT` (111);
//! * tuples — `SMALL_TUPLE_EXT` (104) and `LARGE_TUPLE_EXT` (105, for a >255-element tuple);
//! * lists — `NIL_EXT` (106), `STRING_EXT` (107), `LIST_EXT` (108) (including an
//!   improper tail);
//! * binaries — `BINARY_EXT` (109) and `BIT_BINARY_EXT` (77);
//! * maps — `MAP_EXT` (116); a *multi-key* map is compared through its **sorted** key list,
//!   because map iteration order is an internal detail and not part of the wire contract,
//!   and a flatmap's canonical **key order** is pinned byte-exactly (`wire/map_ext_*`), as is
//!   the rejection of a blob that repeats a key;
//! * atoms — the legacy `ATOM_EXT` (100), and the UTF-8 `ATOM_UTF8_EXT` (118) /
//!   `SMALL_ATOM_UTF8_EXT` (119) including the UTF-8 bytes of a non-Latin-1 atom;
//! * external things — `NEW_PID_EXT` (88), `NEWER_REFERENCE_EXT` (90), `NEW_PORT_EXT` (89)
//!   and `NEWER_PORT_EXT` (120), plus the *legacy* `PID_EXT` (103), `REFERENCE_EXT` (101),
//!   `NEW_REFERENCE_EXT` (114) and `PORT_EXT` (102) forms — all encoded through the
//!   *decode → re-encode* round-trip, which is deterministic inside one node's encoder. The
//!   terms whose bytes depend on the runtime's **own** node identity — a local `self()` pid and
//!   an internal port — carry per-VM-start values and are pinned (by their stable leading shape)
//!   in the [`super::wire_ids`] group instead;
//! * funs/records — `EXPORT_EXT` (113) (encode and decode → re-encode round-trip),
//!   `NEW_FUN_EXT` (112), and the nominal-record `RECORD_EXT` (67) (decode → re-encode);
//! * framing — `VERSION_MAGIC` (131) as the first byte, the `compressed` (80) option (both
//!   directions, including its declared uncompressed-size bound), and `{minor_version, 0|1|2}`;
//! * `ATOM_CACHE_REF` (82) — a blob carrying one is decoded with **no** atom-cache context,
//!   which both runtimes refuse identically.
//!
//! The live handshake / a node-to-node loopback and the `DOP_*` control-message framing are
//! **not** expressible through a single CLI process (they need two connected nodes and the
//! `DFLAG_*` protocol), so they remain an honest, documented gap rather than a fabricated
//! case.
//!
//! ## Honest capability gaps (pinned as `unsupported`, never a mismatch)
//!
//! * The free-standing encode of a `NEW_FUN_EXT` (112) fun is the Rust runtime's remaining
//!   A.5 *encode*-side gap; `wire/new_fun_ext_encode` pins it as `unsupported` (`beam_rt:
//!   unsupported: local_fun`), never a mismatch.
//! * `term_to_binary/2` with `{minor_version, 0|1}` reproduces the *legacy* `ATOM_EXT` (100) /
//!   `FLOAT_EXT` (99) forms, so the `wire/minor_version_*_legacy_encode` cases `agree`.
//!   Decoding a `compressed` (80) blob is covered by the `wire/compressed_decode_*` cases.
//! * The `[local]` option image (`term_to_binary(Term, [local])`) IS served through the BIF: the
//!   image is `131`, `LOCAL_EXT` (121), a 4-byte *local node hash*, then the ordinary ETF body. The
//!   hash is randomised on every node start on **both** sides, so no whole-image byte golden exists;
//!   the `etf/term_to_binary_local_*` cases in [`super::etf`] mask exactly those 4 bytes and compare
//!   the rest of the image byte-for-byte.
//! * The `[deterministic]` option is byte-exact only where **no re-sorting is needed**: a map
//!   already in canonical key order (and any non-map term) encodes identically, while a map
//!   whose flatmap/HAMT order differs from the canonical one is the BIF's typed
//!   `unsupported` — the codec has no deterministic mode. The canonical flatmap key order
//!   itself is pinned by the plain encoder elsewhere in this group.
//! * `term_to_binary(make_ref())` now *executes* on both runtimes (a `NEWER_REFERENCE_EXT`
//!   blob of 35 bytes on each), but the reference id's mask/hash bytes are random per
//!   creation, so the images differ; only the `byte_size/1` shape would be pinnable, and the
//!   image is deliberately left unpinned.
//! * `FUN_EXT` (115) is deliberately not a case: the reference decoder does not validate it
//!   (even a truncated blob decodes to the empty atom `''`), so any golden would pin an
//!   unspecified artifact rather than the wire semantics. The modern `NEW_FUN_EXT` (112) is
//!   the tag that matters for the distribution wire.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.5 cases: the ETF codec used by the distribution wire.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- the original decode/encode cross-checks --------------------------------
        Case {
            id: "wire/decode_reference_tuple_blob",
            description: "an ETF blob produced by the reference runtime decodes to {ok,42}",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,119,2,111,107,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_atom_blob",
            description: "an ETF blob of an atom decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_map_blob",
            description: "an ETF MAP_EXT blob decodes to the original map",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok #{a => 1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encode_atom_matches_reference",
            description: "encoding an atom to ETF reproduces the reference wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(foo)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,6,131,119,3,102,111,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/cross_roundtrip",
            description: "decoding the reference encoding of {ok,42} reproduces the term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_size",
            description: "the reference encoding of {ok,42} is 9 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 9"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_small_int_blob",
            description: "an ETF SMALL_INT_EXT blob decodes to the integer it carries",
            contracts: &[Contract::Distribution],
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
            id: "wire/encode_map_matches_reference",
            description: "encoding a map reproduces the reference MAP_EXT wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_atom_size",
            description: "the reference encoding of the atom foo is 6 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 6"),
                exit_code: 0,
            },
        },
        // -- numbers: NEW_FLOAT_EXT (70) --------------------------------------------
        Case {
            id: "wire/float_decode_reference",
            description: "a fixed NEW_FLOAT_EXT (70) blob decodes to the IEEE-754 double it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,70,64,9,30,184,81,235,133,31>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3.14"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_encode_matches_reference",
            description: "encoding a float reproduces the reference NEW_FLOAT_EXT (70) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-2.5)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,70,192,4,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a float",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(1.5))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1.5"),
                exit_code: 0,
            },
        },
        // -- big integers: SMALL_BIG_EXT (110) / LARGE_BIG_EXT (111) ----------------
        Case {
            id: "wire/small_big_negative_decode_reference",
            description: "a fixed SMALL_BIG_EXT (110) blob with sign byte 1 decodes to a negative big",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,110,3,1,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -65536"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_encode_matches_reference",
            description: "a 7-digit big integer encodes as SMALL_BIG_EXT (110) with little-endian digits",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(65536*65536*65536)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,0,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_negative_encode_matches_reference",
            description: "a negative big integer encodes with the SMALL_BIG_EXT (110) sign byte set",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-(65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,1,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_decode_reference",
            description: "a fixed LARGE_BIG_EXT (111) blob decodes to 2^64",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,111,0,0,0,9,0,0,0,0,0,0,0,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a 9-digit big integer",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(65536*65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        // -- strings / lists: NIL_EXT (106), STRING_EXT (107), LIST_EXT (108) --------
        Case {
            id: "wire/nil_ext_decode_reference",
            description: "the NIL_EXT (106) empty-list blob decodes to []",
            contracts: &[Contract::Distribution],
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
            id: "wire/string_ext_decode_reference",
            description: "a fixed STRING_EXT (107) blob decodes to the byte list it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,107,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/string_ext_encode_matches_reference",
            description: "a byte list encodes as STRING_EXT (107) with a 16-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(\"abc\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_decode_reference",
            description: "a fixed LIST_EXT (108) blob with a NIL_EXT tail decodes to [1,2,3]",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,108,0,0,0,3,97,1,97,2,97,3,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_improper_tail",
            description: "an improper list encodes as LIST_EXT (108) whose tail is the raw element",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1|2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,1,97,1,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a three-element list",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        // -- binaries: BINARY_EXT (109) ---------------------------------------------
        Case {
            id: "wire/binary_ext_encode_matches_reference",
            description: "a binary encodes as BINARY_EXT (109) with a 32-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/binary_ext_decode_reference",
            description: "a fixed BINARY_EXT (109) blob decodes to the byte binary it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,109,0,0,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<97,98,99>>"),
                exit_code: 0,
            },
        },
        // -- atoms: SMALL_ATOM_UTF8_EXT (119) / ATOM_UTF8_EXT (118) ------------------
        Case {
            id: "wire/small_atom_utf8_ext_decode_reference",
            description: "a fixed SMALL_ATOM_UTF8_EXT (119) blob decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_utf8_ext_decode_reference",
            description: "a fixed ATOM_UTF8_EXT (118) blob with a 16-bit length decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,118,0,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_unicode_encode",
            description: "a non-Latin-1 atom encodes as SMALL_ATOM_UTF8_EXT (119) with its UTF-8 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(list_to_atom([945]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,206,177>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a plain atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(hello))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok hello"),
                exit_code: 0,
            },
        },
        // -- external things: NEW_PID_EXT (88), NEWER_REFERENCE_EXT (90), ports -----
        Case {
            id: "wire/new_pid_ext_roundtrip",
            description: "a fixed NEW_PID_EXT (88) blob re-encodes to the identical PID bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_reference_ext_roundtrip",
            description: "a fixed NEWER_REFERENCE_EXT (90) blob re-encodes to the identical reference bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_port_ext_upgrades_to_newer",
            description: "a NEW_PORT_EXT (89) blob re-encodes as the equivalent NEWER_PORT_EXT (120) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,89,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_port_ext_roundtrip",
            description: "a fixed NEWER_PORT_EXT (120) blob re-encodes to the identical port bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/legacy_port_ext_width_roundtrip",
            description: "a legacy PORT_EXT (102) blob (32-bit id + 1-byte creation) decodes and re-encodes as the equivalent NEWER_PORT_EXT (120) image with the id and creation preserved",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // A *fixed external-node* port (node `other`, id 7, creation 1): every field
                // is byte-stable, so the decode -> re-encode image is pinnable.
                expr: "binary_to_term(<<131,102,100,0,5,111,116,104,101,114,0,0,0,7,1>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,1>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/legacy_port_ext_creation_zero_roundtrip",
            description: "a legacy PORT_EXT (102) blob with creation 0 decodes and re-encodes as the equivalent NEWER_PORT_EXT (120) image with the zero creation preserved",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,102,100,0,5,111,116,104,101,114,0,0,0,7,0>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        // -- funs: EXPORT_EXT (113) / NEW_FUN_EXT (112) -----------------------------
        Case {
            id: "wire/export_ext_encode",
            description: "an exported function fun encodes as EXPORT_EXT (113) (module/function/arity)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun lists:reverse/1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_fun_ext_encode",
            description: "a lambda encodes as NEW_FUN_EXT (112) with its module, index and free variables",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun() -> ok end)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,112,0,0,0,136,0,248,37,47,246,123,244,211,6,126,138,154,65,205,61,207,38,0,0,0,43,0,0,0,1,119,8,101,114,108,95,101,118,97,108,97,43,98,7,193,41,127,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,0,0,0,0,0,0,0,104,6,97,1,106,119,4,110,111,110,101,119,4,110,111,110,101,116,0,0,0,0,108,0,0,0,1,104,5,119,6,99,108,97,117,115,101,97,1,106,106,108,0,0,0,1,104,3,119,4,97,116,111,109,97,1,119,2,111,107,106,106>>"),
                exit_code: 0,
            },
        },
        // -- compression / version framing ------------------------------------------
        Case {
            id: "wire/compressed_option_encode",
            description: "term_to_binary/2 with the compressed option encodes the same term under both runtimes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1,2,3], [compressed])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,1,2,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_option_roundtrip",
            description: "term_to_binary/2 with the compressed option round-trips through binary_to_term/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3], [compressed]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/version_magic_first_byte",
            description: "every encoded term starts with VERSION_MAGIC (131)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary:first(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 131"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_2_map",
            description: "term_to_binary/2 with {minor_version,2} encodes a map as MAP_EXT",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1}, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        // -- atom cache: ATOM_CACHE_REF (82) ----------------------------------------
        Case {
            id: "wire/atom_cache_ref_without_context",
            description: "a blob carrying ATOM_CACHE_REF (82) is refused identically without a cache context",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,82,0,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // -- NEW_REFERENCE_EXT (114): the compact reference form --------------------
        Case {
            id: "wire/new_reference_ext_is_reference",
            description: "a NEW_REFERENCE_EXT (114) blob decodes to a reference",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "is_reference(binary_to_term(<<131,114,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_reference_ext_roundtrip",
            description: "a NEW_REFERENCE_EXT (114) blob re-encodes as the equivalent NEWER_REFERENCE_EXT (90) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,114,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,0,0,0,2,35,202,52,184,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        // -- BIT_BINARY_EXT (77): bitstrings with a bit count -----------------------
        Case {
            id: "wire/bit_binary_ext_decode",
            description: "a fixed BIT_BINARY_EXT (77) blob decodes to the bitstring it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,77,0,0,0,1,3,96>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<3:3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/bit_binary_ext_encode",
            description: "a 3-bit bitstring encodes as BIT_BINARY_EXT (77) with its bit count",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<3:3>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,77,0,0,0,1,3,96>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/bit_binary_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a bitstring",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(<<3:3>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<3:3>>"),
                exit_code: 0,
            },
        },
        // -- tuples: SMALL_TUPLE_EXT (104) / LARGE_TUPLE_EXT (105) -------------------
        Case {
            id: "wire/small_tuple_ext_encode",
            description: "a 2-tuple encodes as SMALL_TUPLE_EXT (104) with a 32-bit arity",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary({a,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_tuple_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a tuple",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,1}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {a,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_tuple_ext_encode_tag",
            description: "a 300-element tuple encodes with the LARGE_TUPLE_EXT (105) tag",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary:at(term_to_binary(list_to_tuple(lists:seq(1,300))), 1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 105"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_tuple_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a 300-element tuple",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "tuple_size(binary_to_term(term_to_binary(list_to_tuple(lists:seq(1,300)))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 300"),
                exit_code: 0,
            },
        },
        // -- MAP_EXT (116): a multi-key map, compared order-independently ------------
        Case {
            id: "wire/map_ext_multi_key_roundtrip",
            description: "a multi-key map round-trips through MAP_EXT (116), compared as a sorted key list",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "lists:sort(maps:to_list(binary_to_term(term_to_binary(#{z => 1, a => 2, m => 3}))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,2},{m,3},{z,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_noncanonical_key_order_canonicalizes",
            description: "a MAP_EXT (116) flatmap whose keys are on the wire in non-canonical order decodes and re-encodes in canonical sorted-key order",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // `#{2 => 20, 1 => 10}` on the wire as `2` then `1`.
                expr: "binary_to_term(<<131,116,0,0,0,2,97,2,97,20,97,1,97,10>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,2,97,1,97,10,97,2,97,20>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_duplicate_key_badarg",
            description: "a MAP_EXT (116) blob that repeats a key is rejected with badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,116,0,0,0,2,97,1,97,10,97,1,97,20>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/map_ext_canonical_key_order_float_list_binary",
            description: "term_to_binary/1 of a map with a float, a list and a binary key emits the canonical term-order flatmap image byte-exactly",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "#{1.5 => a, [1,2] => b, <<\"z\">> => c}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,116,0,0,0,3,70,63,248,0,0,0,0,0,0,119,1,97,107,0,2,1,2,119,1,98,109,0,0,0,1,122,119,1,99>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_binary_key_order",
            description: "term_to_binary/1 orders several binary keys canonically in the flatmap image",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "#{<<\"b\">> => 1, <<\"a\">> => 2, <<\"c\">> => 3, <<\"aa\">> => 4}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,116,0,0,0,4,109,0,0,0,1,97,97,2,109,0,0,0,2,97,97,97,4,109,0,0,0,1,98,97,1,109,0,0,0,1,99,97,3>>",
                ),
                exit_code: 0,
            },
        },
        // -- legacy integer / atom forms: INTEGER_EXT (98), ATOM_EXT (100) -----------
        Case {
            id: "wire/integer_ext_legacy_decode",
            description: "a fixed legacy INTEGER_EXT (98) blob decodes to the 32-bit integer it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,98,0,0,1,44>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 300"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_ext_legacy_decode",
            description: "a fixed legacy ATOM_EXT (100) blob decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,100,0,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        // -- legacy external forms: PID_EXT (103) / REFERENCE_EXT (101) --------------
        Case {
            id: "wire/pid_ext_upgrades_to_new_pid",
            description: "a legacy PID_EXT (103) blob re-encodes as the equivalent NEW_PID_EXT (88) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,103,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,88,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/reference_ext_upgrades_to_newer",
            description: "a legacy REFERENCE_EXT (101) blob re-encodes as the equivalent NEWER_REFERENCE_EXT (90) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,101,119,7,102,111,111,64,98,97,114,0,0,0,1,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,1,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1>>",
                ),
                exit_code: 0,
            },
        },
        // -- EXPORT_EXT (113): the decode direction of an exported fun --------------
        Case {
            id: "wire/export_ext_decode_roundtrip",
            description: "a fixed EXPORT_EXT (113) blob decodes to the exported fun and re-encodes identically",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>",
                ),
                exit_code: 0,
            },
        },
        // -- `{minor_version, _}`: the wire dialect of the encode ---------------------
        // `N < 2` selects the *legacy* `ATOM_EXT` (100) / `FLOAT_EXT` (99) forms; both
        // runtimes encode them identically, so these cases `agree`.
        Case {
            id: "wire/minor_version_0_atom_legacy_encode",
            description: "term_to_binary/2 with {minor_version,0} encodes an atom as the legacy ATOM_EXT (100)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,9,131,100,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_1_atom_legacy_encode",
            description: "term_to_binary/2 with {minor_version,1} still encodes an atom as the legacy ATOM_EXT (100)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [{minor_version, 1}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,9,131,100,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_0_float_legacy_encode",
            description: "term_to_binary/2 with {minor_version,0} encodes a float as the legacy ASCII FLOAT_EXT (99)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,109,0,0,0,33,131,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_1_float_encode",
            description: "term_to_binary/2 with {minor_version,1} encodes a float as NEW_FLOAT_EXT (70)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 1}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,10,131,70,63,248,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        // -- `compressed` (80) DECODE -------------------------------------------------
        Case {
            id: "wire/compressed_decode_roundtrip_large",
            description: "a large term round-trips through term_to_binary/2 with the compressed option and binary_to_term/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(lists:seq(1,1000), [compressed])) =:= lists:seq(1,1000)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_decode_trailing_bytes_ignored",
            description: "a fixed COMPRESSED (80) blob whose inflated body carries trailing bytes after a complete term decodes to that term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // Compressed body = version-magic-stripped external {ok,42} ++ <<1,2,3,4,5>>.
                expr: "binary_to_term(<<131,80,0,0,0,13,120,156,203,96,42,103,202,207,78,212,98,100,98,102,97,5,0,21,163,2,88>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,2,111,107,97,42>>"),
                exit_code: 0,
            },
        },
        // -- COMPRESSED (80) declared-inner-size bound --------------------------------
        Case {
            id: "wire/compressed_declared_size_bound",
            description: "a COMPRESSED (80) image declared with its true uncompressed size round-trips",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // Rebuild the blob from its own declared size so the bound is exactly met.
                expr: "begin T = binary:copy(<<\"abcdabcd\">>,400), Comp = term_to_binary(T,[compressed]), <<131,80,Sz:32,Tail/binary>> = Comp, binary_to_term(<<131,80,Sz:32,Tail/binary>>) =:= T end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_declared_size_mismatch_badarg",
            description: "a COMPRESSED (80) image whose declared uncompressed size is one byte larger than the true size is rejected with badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "begin T = binary:copy(<<\"abcdabcd\">>,400), Comp = term_to_binary(T,[compressed]), <<131,80,Sz:32,Tail/binary>> = Comp, binary_to_term(<<131,80,(Sz+1):32,Tail/binary>>) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // -- RECORD_EXT (67): the nominal-record instance form ------------------------
        Case {
            id: "wire/record_ext_roundtrip",
            description: "a fixed RECORD_EXT (67) blob decodes to the record instance and re-encodes to identical bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,67,0,0,0,2,0,119,5,109,121,109,111,100,119,6,112,101,114,115,111,110,119,4,110,97,109,101,119,3,97,103,101,119,3,106,111,101,97,21>>))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,109,0,0,0,40,131,67,0,0,0,2,0,119,5,109,121,109,111,100,119,6,112,101,114,115,111,110,119,4,110,97,109,101,119,3,97,103,101,119,3,106,111,101,97,21>>",
                ),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::Contract;

    #[test]
    fn new_wire_cases_are_registered_under_the_distribution_contract() {
        let cases = group();
        for id in [
            "wire/minor_version_0_atom_legacy_encode",
            "wire/minor_version_1_atom_legacy_encode",
            "wire/minor_version_0_float_legacy_encode",
            "wire/minor_version_1_float_encode",
            "wire/compressed_decode_roundtrip_large",
            "wire/compressed_decode_trailing_bytes_ignored",
            "wire/record_ext_roundtrip",
            "wire/legacy_port_ext_width_roundtrip",
            "wire/legacy_port_ext_creation_zero_roundtrip",
            "wire/map_ext_noncanonical_key_order_canonicalizes",
            "wire/map_ext_canonical_key_order_float_list_binary",
            "wire/map_ext_binary_key_order",
            "wire/compressed_declared_size_bound",
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::Distribution], "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
    }

    #[test]
    fn exception_wire_cases_raise_badarg_under_the_distribution_contract() {
        let cases = group();
        for id in [
            "wire/map_ext_duplicate_key_badarg",
            "wire/compressed_declared_size_mismatch_badarg",
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::Distribution], "{id}");
            assert_eq!(case.golden.stdout, Some("exception error badarg"), "{id}");
            assert_eq!(case.golden.exit_code, 1, "{id}");
        }
    }
}
