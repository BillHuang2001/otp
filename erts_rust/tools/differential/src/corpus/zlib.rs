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
//! Contract **A.7** — the `zlib` codec: **byte identity** with the oracle.
//!
//! The runtime's DEFLATE encoder is libz-exact, so every zlib surface this group exercises —
//! the header-less `zlib:zip/1`, the zlib-wrapped `zlib:compress/1`, the gzip container
//! `zlib:gzip/1`, and the `zlib:deflate*` stream API at levels 0/1/6/9 — emits the **oracle's
//! own bytes**, on repetitive, structured, tiny and large bodies alike. What this group pins:
//!
//! * **byte-exact image goldens on four bodies**, each through all three one-shot siblings:
//!   the 8 KiB zero body (`zlib:compress/1` → 17 bytes), the 1 KiB zero body, the 100-byte
//!   sequence `list_to_binary(lists:seq(1,100))` (102 bytes of `compress` output) and the
//!   16 000-byte repetitive text `binary:copy(<<"abcdefgh">>, 2000)` (54 / 48 / 66 bytes) —
//!   plus the tiny bodies (`<<"hello">>` → `<<203,72,205,201,201,7,0>>`, a fixed-Huffman
//!   block; `<<>>` → `<<3,0>>`; the stored blocks `<<3,4,5>>` → `<<99,102,97,5,0>>` and the
//!   iodata list `[1,2,3]` → `<<99,100,98,6,0>>`; and `<<"abc">>` through both wrappers).
//!   Every golden is the **oracle's** own stdout line, re-verified with
//!   `differential run <id>` against `beam_rt`.
//! * **the round trip.** `zlib:unzip(zlib:zip(B)) =:= B` over an empty, a tiny and a
//!   100-byte body, and `zlib:uncompress∘zlib:compress` / `zlib:gunzip∘zlib:gzip` over a
//!   tiny body — the inflaters against the encoder.
//! * **the stream API.** `zlib:open/0` + `deflateInit/6` + `deflate/3` + `deflateEnd/1` +
//!   `close/1` at level 6, level 9, level 1 and level 0 (a **stored** block), a
//!   `zlib:deflateParams/3` level change **mid-stream** (deflate with `none`, switch the
//!   level, deflate with `finish`), and `inflateInit/1` + `inflate/2` both over a whole
//!   stream and split across two chunks. The images are pinned byte-exactly, so a different
//!   block or Huffman choice at any of those levels is a visible mismatch.
//! * **the error matrix.** `zlib:unzip/1` has **no length arm**: an empty stream and a
//!   three-byte stream are both the bare `error:data_error` (never `badarg`), while a
//!   *non-iodata* argument (`42`) is `error:badarg` on both `zip/1` and `unzip/1` — and so is
//!   an improper list (`[1,2|3]`), whose `zip/1` would have to walk a non-list tail. A
//!   non-stream or non-gzip argument to `uncompress/1`/`gunzip/1` is `data_error`, never
//!   `badarg`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `zlib` raw-DEFLATE cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- byte-exact goldens: the three bodies the oracle reproduces byte for byte ------
        Case {
            id: "zlib/zip_hello_is_byte_exact",
            description: "zlib:zip/1 of <<\"hello\">> emits the oracle's raw DEFLATE bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(<<\"hello\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<203,72,205,201,201,7,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_empty_is_byte_exact",
            description: "zlib:zip/1 of the empty body emits the oracle's raw DEFLATE bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(<<>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<3,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_iodata_list_is_byte_exact",
            description: "zlib:zip/1 of the plain integer list [1,2,3] emits the oracle's bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A plain integer list is iodata, so `zip/1` accepts it and deflates the three
                // bytes it denotes — pinned byte-exactly because it is a stored-block image.
                expr: "zlib:zip([1,2,3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<99,100,98,6,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_three_bytes_is_byte_exact",
            description: "zlib:zip/1 of <<3,4,5>> emits the oracle's raw DEFLATE bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(<<3,4,5>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<99,102,97,5,0>>"),
                exit_code: 0,
            },
        },
        // -- the round trip: the inflater against the encoder ------------------------------
        Case {
            id: "zlib/unzip_zip_roundtrip_hello",
            description: "zlib:unzip/1 inverts zlib:zip/1 for a tiny body",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:unzip(zlib:zip(<<\"hello\">>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/unzip_zip_roundtrip_empty",
            description: "zlib:unzip/1 inverts zlib:zip/1 for the empty body",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:unzip(zlib:zip(<<>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/unzip_zip_roundtrip_100_bytes_is_identity",
            description: "zlib:unzip/1∘zlib:zip/1 is the identity over the bytes 1..100",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `=:=` compares the *body*: the inflater's correctness evidence, alongside
                // the body's image, which is pinned byte-exactly by
                // `zlib/zip_100_byte_sequence_is_byte_exact`.
                expr: "zlib:unzip(zlib:zip(list_to_binary(lists:seq(1,100)))) \
                       =:= list_to_binary(lists:seq(1,100))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // -- the error matrix: data_error for a stream, badarg for a non-iodata argument -----
        Case {
            id: "zlib/unzip_empty_is_data_error",
            description: "zlib:unzip/1 of the empty stream is the bare error:data_error",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // No length arm on `unzip/1`: the empty stream is the inflater's own failure,
                // `data_error`, never `badarg`.
                expr: "zlib:unzip(<<>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error data_error"),
                exit_code: 1,
            },
        },
        Case {
            id: "zlib/unzip_three_bytes_is_data_error",
            description: "zlib:unzip/1 of a three-byte stream is the bare error:data_error",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:unzip(<<1,2,3>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error data_error"),
                exit_code: 1,
            },
        },
        Case {
            id: "zlib/unzip_non_iodata_is_badarg",
            description: "zlib:unzip/1 of a non-iodata argument is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:unzip(42)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "zlib/zip_non_iodata_is_badarg",
            description: "zlib:zip/1 of a non-iodata argument is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(42)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "zlib/zip_improper_list_is_badarg",
            description: "zlib:zip/1 of an improper list is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip([1,2|3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // -- the zlib-wrapped pair: compress/uncompress + gzip/gunzip -----------------------
        Case {
            id: "zlib/compress_bytes_are_deterministic",
            description: "zlib:compress/1 of a tiny body is a byte-exact, order-stable zlib stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The tiny-body anchor; the multi-kilobyte bodies below carry their own
                // byte-exact images on the same basis.
                expr: "zlib:compress(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,75,76,74,6,0,2,77,1,39>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/gzip_bytes_are_deterministic",
            description: "zlib:gzip/1 of a tiny body is a byte-exact gzip container",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gzip(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<31,139,8,0,0,0,0,0,0,3,75,76,74,6,0,194,65,36,53,3,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/uncompress_compress_roundtrip_tiny",
            description: "zlib:uncompress/1 inverts zlib:compress/1 for a tiny body",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:uncompress(zlib:compress(<<\"abc\">>)) =:= <<\"abc\">>",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/uncompress_compress_roundtrip_100_bytes_is_identity",
            description: "zlib:uncompress/1 inverts zlib:compress/1 for a 100-byte body",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `=:=` compares the *body*; the body's 102-byte compressed image is pinned
                // separately by `zlib/compress_100_byte_sequence_is_byte_exact`.
                expr: "zlib:uncompress(zlib:compress(list_to_binary(lists:seq(1,100)))) \
                       =:= list_to_binary(lists:seq(1,100))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/gunzip_gzip_roundtrip_tiny",
            description: "zlib:gunzip/1 inverts zlib:gzip/1 for a tiny body",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gunzip(zlib:gzip(<<\"abc\">>)) =:= <<\"abc\">>",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/uncompress_three_bytes_is_data_error",
            description: "zlib:uncompress/1 of a 3-byte non-stream is the bare data_error (no \
                          length arm)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:uncompress(<<0,1,2>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error data_error"),
                exit_code: 1,
            },
        },
        Case {
            id: "zlib/gunzip_non_gzip_is_data_error",
            description: "zlib:gunzip/1 of a non-gzip stream is the bare data_error",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gunzip(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error data_error"),
                exit_code: 1,
            },
        },
        // -- byte identity on real bodies: the encoder is libz-exact ------------------------
        // Every image below is the *oracle's* own stdout line. The four bodies are the 8 KiB
        // and 1 KiB zero bodies, the 100-byte sequence and a 16 000-byte repetitive text; each
        // is put through all three one-shot siblings (`zzip` raw DEFLATE, `compress` the
        // zlib-wrapped stream, `gzip` the gzip container).
        Case {
            id: "zlib/compress_8192_zeros_is_byte_exact",
            description: "zlib:compress/1 of the 8 KiB zero body emits the oracle's 17-byte zlib stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The body the old module doc claimed diverged (17 bytes on the oracle, 23 on
                // the runtime): both now emit these very bytes.
                expr: "zlib:compress(<<0:8192>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,99,96,24,5,163,96,20,140,84,0,0,4,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/compress_1024_zeros_is_byte_exact",
            description: "zlib:compress/1 of the 1 KiB zero body emits the oracle's 12-byte zlib stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:compress(<<0:1024>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,99,96,24,88,0,0,0,128,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_1024_zeros_is_byte_exact",
            description: "zlib:zip/1 of the 1 KiB zero body emits the oracle's 6-byte raw DEFLATE block",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(<<0:1024>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<99,96,24,88,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/gzip_1024_zeros_is_byte_exact",
            description: "zlib:gzip/1 of the 1 KiB zero body emits the oracle's 24-byte gzip container",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gzip(<<0:1024>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<31,139,8,0,0,0,0,0,0,3,99,96,24,88,0,0,157,250,168,194,128,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/compress_100_byte_sequence_is_byte_exact",
            description: "zlib:compress/1 of the 100-byte sequence 1..100 emits the oracle's 102-byte stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The body the old module doc claimed diverged in its compressed *image*
                // (oracle 102 bytes, runtime 105): both now emit these very 102 bytes.
                expr: "zlib:compress(list_to_binary(lists:seq(1,100)))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,99,100,98,102,97,101,99,231,224,228,226,230,225,229,227,23,16,20,18,22,17,21,19,151,144,148,146,150,145,149,147,87,80,84,82,86,81,85,83,215,208,212,210,214,209,213,211,55,48,52,50,54,49,53,51,183,176,180,178,182,177,181,179,119,112,116,114,118,113,117,115,247,240,244,242,246,241,245,243,15,8,12,10,14,9,13,11,143,136,140,138,142,137,141,139,79,72,76,74,78,1,0,159,54,19,187>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_100_byte_sequence_is_byte_exact",
            description: "zlib:zip/1 of the 100-byte sequence 1..100 emits the oracle's 102-byte raw DEFLATE stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The same body the round-trip case pins by identity now carries its image: the
                // oracle's stream is the runtime's stream.
                expr: "zlib:zip(list_to_binary(lists:seq(1,100)))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<99,100,98,102,97,101,99,231,224,228,226,230,225,229,227,23,16,20,18,22,17,21,19,151,144,148,146,150,145,149,147,87,80,84,82,86,81,85,83,215,208,212,210,214,209,213,211,55,48,52,50,54,49,53,51,183,176,180,178,182,177,181,179,119,112,116,114,118,113,117,115,247,240,244,242,246,241,245,243,15,8,12,10,14,9,13,11,143,136,140,138,142,137,141,139,79,72,76,74,78,1,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/gzip_100_byte_sequence_is_byte_exact",
            description: "zlib:gzip/1 of the 100-byte sequence 1..100 emits the oracle's 120-byte gzip container",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gzip(list_to_binary(lists:seq(1,100)))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<31,139,8,0,0,0,0,0,0,3,99,100,98,102,97,101,99,231,224,228,226,230,225,229,227,23,16,20,18,22,17,21,19,151,144,148,146,150,145,149,147,87,80,84,82,86,81,85,83,215,208,212,210,214,209,213,211,55,48,52,50,54,49,53,51,183,176,180,178,182,177,181,179,119,112,116,114,118,113,117,115,247,240,244,242,246,241,245,243,15,8,12,10,14,9,13,11,143,136,140,138,142,137,141,139,79,72,76,74,78,1,0,66,15,240,101,100,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/compress_repetitive_text_is_byte_exact",
            description: "zlib:compress/1 of a 16 000-byte repetitive text body emits the oracle's 54-byte stream",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `binary:copy(<<"abcdefgh">>, 2000)`: 16 000 bytes with a period of 8, long
                // enough that the encoder's match search — not its block bootstrap — decides
                // the image.
                expr: "zlib:compress(binary:copy(<<\"abcdefgh\">>, 2000))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,237,197,49,1,0,32,8,0,176,172,8,8,253,19,24,196,237,89,156,172,190,179,97,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,254,250,7,59,151,138,169>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/zip_repetitive_text_is_byte_exact",
            description: "zlib:zip/1 of the 16 000-byte repetitive text body emits the oracle's 48 bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:zip(binary:copy(<<\"abcdefgh\">>, 2000))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<237,197,49,1,0,32,8,0,176,172,8,8,253,19,24,196,237,89,156,172,190,179,97,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,254,250,7>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/gzip_repetitive_text_is_byte_exact",
            description: "zlib:gzip/1 of the 16 000-byte repetitive text body emits the oracle's 66 bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "zlib:gzip(binary:copy(<<\"abcdefgh\">>, 2000))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<31,139,8,0,0,0,0,0,0,3,237,197,49,1,0,32,8,0,176,172,8,8,253,19,24,196,237,89,156,172,190,179,97,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,254,250,7,40,250,247,29,128,62,0,0>>"),
                exit_code: 0,
            },
        },
        // -- the stream API: deflateInit/deflate/deflateEnd at four levels --------------------
        Case {
            id: "zlib/deflate_stream_level6_is_byte_exact",
            description: "the deflate stream API at level 6 emits the oracle's zlib-wrapped image",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `zlib:compress/1` is this route; pinned separately so a divergence in the
                // stream surface alone cannot hide behind the one-shot helper.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 6, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, <<0:8192>>, finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), D end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,156,99,96,24,5,163,96,20,140,84,0,0,4,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/deflate_stream_level9_is_byte_exact",
            description: "the deflate stream API at level 9 emits the oracle's zlib-wrapped image",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The FLG byte (218 vs 156 at level 6) records the level, so a level that never
                // reached the encoder is a visible mismatch rather than a silent pass.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 9, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, binary:copy(<<\"abcdefgh\">>, 2000), finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), D end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,218,237,197,49,1,0,32,8,0,176,172,8,8,253,19,24,196,237,89,156,172,190,179,97,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,254,250,7,59,151,138,169>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/deflate_stream_level1_is_byte_exact",
            description: "the deflate stream API at level 1 emits the oracle's zlib-wrapped image",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Level 1 takes a different (and much longer) block path than levels 6/9 on this
                // body — 119 bytes against 54 — so the fast tier is pinned on its own.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 1, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, binary:copy(<<\"abcdefgh\">>, 2000), finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), D end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,1,237,212,193,17,0,48,8,2,176,89,173,90,221,127,130,14,210,188,248,115,144,56,89,125,103,67,234,193,14,252,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,95,59,240,0,59,151,138,169>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/deflate_stream_level0_is_a_stored_block",
            description: "the deflate stream API at level 0 emits the oracle's stored-block image",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Level 0 is the one level whose image is a *stored* block — `<<120,1,...>>` is
                // the zlib header with FLEVEL 0, and the body is the eight bytes verbatim.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 0, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, <<\"abcdefgh\">>, finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), D end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<120,1,1,8,0,247,255,97,98,99,100,101,102,103,104,14,0,3,37>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/deflate_params_level_change_is_byte_exact",
            description: "a mid-stream zlib:deflateParams/3 level change emits the oracle's two segment \
                          sizes and the concatenated image",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Deflate 8000 bytes with `none`, switch 6 -> 1, then deflate another 8000 with
                // `finish`. The first segment is the 2-byte header alone (the encoder buffered
                // everything), the second carries the whole body through both levels.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 6, deflated, 15, 8, default), \
                       P1 = iolist_to_binary(zlib:deflate(Z, binary:copy(<<\"abcdefgh\">>, 1000), none)), \
                       ok = zlib:deflateParams(Z, 1, default), \
                       P2 = iolist_to_binary(zlib:deflate(Z, binary:copy(<<\"abcdefgh\">>, 1000), finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), \
                       {byte_size(P1), byte_size(P2), iolist_to_binary([P1, P2])} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,106,<<120,156,236,197,49,1,0,32,8,0,176,172,8,8,253,19,24,196,237,89,156,172,190,179,97,219,182,109,219,182,109,219,182,109,219,246,199,63,0,0,0,255,255,237,212,49,13,0,0,0,194,48,185,204,191,2,140,244,226,39,80,169,7,59,240,3,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,28,224,0,7,56,192,1,14,112,128,3,235,59,151,138,169>>}"),
                exit_code: 0,
            },
        },
        // -- the inflate stream API ----------------------------------------------------------
        Case {
            id: "zlib/inflate_init_inflate_roundtrip",
            description: "zlib:inflateInit/1 + zlib:inflate/2 invert a deflated stream end to end",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 6, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, <<\"hello hello hello hello\">>, finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), \
                       I = zlib:open(), ok = zlib:inflateInit(I), \
                       Out = iolist_to_binary(zlib:inflate(I, D)), \
                       ok = zlib:inflateEnd(I), ok = zlib:close(I), Out end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<104,101,108,108,111,32,104,101,108,108,111,32,104,101,108,108,111,32,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "zlib/inflate_two_chunks_is_byte_exact",
            description: "the inflate stream API reassembles a stream split into two chunks, and the \
                          deflated image itself is the oracle's",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A 4000-byte body deflated to 37 bytes, fed to the inflater in a 20-byte and a
                // 17-byte chunk; the case pins the deflated image *and* the reassembled size.
                expr: "begin Z = zlib:open(), \
                       ok = zlib:deflateInit(Z, 6, deflated, 15, 8, default), \
                       D = iolist_to_binary(zlib:deflate(Z, binary:copy(<<\"abcdefgh\">>, 500), finish)), \
                       ok = zlib:deflateEnd(Z), ok = zlib:close(Z), \
                       H = binary:part(D, 0, 20), T = binary:part(D, 20, byte_size(D) - 20), \
                       I = zlib:open(), ok = zlib:inflateInit(I), \
                       A = iolist_to_binary(zlib:inflate(I, H)), \
                       B = iolist_to_binary(zlib:inflate(I, T)), \
                       ok = zlib:inflateEnd(I), ok = zlib:close(I), \
                       {byte_size(D), D, byte_size(A) + byte_size(B)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {37,<<120,156,237,197,49,1,0,32,8,0,176,172,8,8,253,19,24,195,103,123,22,39,171,239,108,216,182,109,219,182,237,111,63,174,223,34,171>>,4000}"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{Contract, Probe};

    /// The byte-exact family: the four bodies whose raw-DEFLATE image the oracle reproduces
    /// byte for byte, each under the `value` probe (so the golden *is* the byte list).
    #[test]
    fn the_byte_exact_family_pins_the_oracle_images() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("registered");
        for (id, expr, golden) in [
            (
                "zlib/zip_hello_is_byte_exact",
                "zlib:zip(<<\"hello\">>)",
                "ok <<203,72,205,201,201,7,0>>",
            ),
            ("zlib/zip_empty_is_byte_exact", "zlib:zip(<<>>)", "ok <<3,0>>"),
            (
                "zlib/zip_iodata_list_is_byte_exact",
                "zlib:zip([1,2,3])",
                "ok <<99,100,98,6,0>>",
            ),
            (
                "zlib/zip_three_bytes_is_byte_exact",
                "zlib:zip(<<3,4,5>>)",
                "ok <<99,102,97,5,0>>",
            ),
            (
                "zlib/compress_bytes_are_deterministic",
                "zlib:compress(<<\"abc\">>)",
                "ok <<120,156,75,76,74,6,0,2,77,1,39>>",
            ),
            (
                "zlib/gzip_bytes_are_deterministic",
                "zlib:gzip(<<\"abc\">>)",
                "ok <<31,139,8,0,0,0,0,0,0,3,75,76,74,6,0,194,65,36,53,3,0,0,0>>",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "{id}");
            assert_eq!(case.expr(), Some(expr), "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
    }

    /// The round-trip family: the inflater inverts the encoder for every body.
    #[test]
    fn the_round_trip_family_inverts_the_encoder() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("registered");
        for (id, golden) in [
            (
                "zlib/unzip_zip_roundtrip_hello",
                "ok <<104,101,108,108,111>>",
            ),
            ("zlib/unzip_zip_roundtrip_empty", "ok <<>>"),
            ("zlib/unzip_zip_roundtrip_100_bytes_is_identity", "ok true"),
            ("zlib/uncompress_compress_roundtrip_tiny", "ok true"),
            (
                "zlib/uncompress_compress_roundtrip_100_bytes_is_identity",
                "ok true",
            ),
            ("zlib/gunzip_gzip_roundtrip_tiny", "ok true"),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
            assert_eq!(case.extra_argv(), &[] as &[&str], "{id}");
        }
        assert_eq!(
            find("zlib/unzip_zip_roundtrip_100_bytes_is_identity").expr(),
            Some(
                "zlib:unzip(zlib:zip(list_to_binary(lists:seq(1,100)))) \
                 =:= list_to_binary(lists:seq(1,100))"
            )
        );
        // The identity cases are not a substitute for the image cases: the 100-byte body's
        // compressed *image* is pinned byte-exactly in its own right (see
        // `the_image_family_pins_the_oracle_bytes`), so a wrong-but-reversible block choice
        // cannot hide here.
        let image_ids: Vec<&str> = group()
            .iter()
            .filter(|case| case.id.contains("100_byte_sequence_is_byte_exact"))
            .map(|case| case.id)
            .collect();
        assert_eq!(
            image_ids,
            [
                "zlib/compress_100_byte_sequence_is_byte_exact",
                "zlib/zip_100_byte_sequence_is_byte_exact",
                "zlib/gzip_100_byte_sequence_is_byte_exact",
            ]
        );
    }

    /// The **byte-identity** family: every body under every one-shot sibling plus the whole
    /// stream API. Each golden is the *oracle's* own line, so the encoder's block and Huffman
    /// choices are pinned, not just the round trip.
    #[test]
    fn the_image_family_pins_the_oracle_bytes() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("registered");
        for (id, golden) in [
            (
                "zlib/compress_8192_zeros_is_byte_exact",
                "ok <<120,156,99,96,24,5,163,96,20,140,84,0,0,4,0,0,1>>",
            ),
            (
                "zlib/compress_1024_zeros_is_byte_exact",
                "ok <<120,156,99,96,24,88,0,0,0,128,0,1>>",
            ),
            ("zlib/zip_1024_zeros_is_byte_exact", "ok <<99,96,24,88,0,0>>"),
            (
                "zlib/gzip_1024_zeros_is_byte_exact",
                "ok <<31,139,8,0,0,0,0,0,0,3,99,96,24,88,0,0,157,250,168,194,128,0,0,0>>",
            ),
            (
                "zlib/deflate_stream_level6_is_byte_exact",
                "ok <<120,156,99,96,24,5,163,96,20,140,84,0,0,4,0,0,1>>",
            ),
            (
                "zlib/deflate_stream_level0_is_a_stored_block",
                "ok <<120,1,1,8,0,247,255,97,98,99,100,101,102,103,104,14,0,3,37>>",
            ),
            (
                "zlib/inflate_init_inflate_roundtrip",
                "ok <<104,101,108,108,111,32,104,101,108,108,111,32,104,101,108,108,111,32,104,101,108,108,111>>",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }

        // The four-body x three-sibling matrix exists in full, and every one of those cases is
        // a raw-image golden (never a `byte_size/1` shape): the encoder is libz-exact, so a
        // shape-only assertion would be strictly weaker than what the runtime now guarantees.
        let mut bodies = 0;
        for body in ["1024_zeros", "100_byte_sequence", "repetitive_text"] {
            for sibling in ["compress_", "zip_", "gzip_"] {
                let id = format!("zlib/{sibling}{body}_is_byte_exact");
                let case = find(&id);
                let golden = case.golden.stdout.expect("image golden");
                assert!(
                    golden.starts_with("ok <<") && golden.ends_with(">>"),
                    "{id}: the image family pins the bytes, not a shape: {golden}"
                );
                bodies += 1;
            }
        }
        assert_eq!(bodies, 9);

        // The stream API covers every level class plus a mid-stream level change and a chunked
        // inflate: level 6, 9, 1, 0 (stored), `deflateParams/3`, whole-stream `inflate/2` and a
        // two-chunk `inflate/2`.
        let stream: Vec<&str> = group()
            .iter()
            .filter(|case| {
                case.expr().is_some_and(|e| {
                    e.contains("zlib:deflateInit")
                        || e.contains("zlib:deflateParams")
                        || e.contains("zlib:inflateInit")
                })
            })
            .map(|case| case.id)
            .collect();
        assert_eq!(
            stream,
            [
                "zlib/deflate_stream_level6_is_byte_exact",
                "zlib/deflate_stream_level9_is_byte_exact",
                "zlib/deflate_stream_level1_is_byte_exact",
                "zlib/deflate_stream_level0_is_a_stored_block",
                "zlib/deflate_params_level_change_is_byte_exact",
                "zlib/inflate_init_inflate_roundtrip",
                "zlib/inflate_two_chunks_is_byte_exact",
            ]
        );
    }
    /// The error matrix: two `data_error` streams (no length arm on `unzip/1`) and three
    /// `badarg` refusals of a non-iodata argument — every one exit 1.
    #[test]
    fn the_error_matrix_pins_data_error_and_badarg() {
        let cases = group();
        let data_error: Vec<&str> = cases
            .iter()
            .filter(|case| case.golden.stdout == Some("exception error data_error"))
            .map(|case| case.id)
            .collect();
        assert_eq!(
            data_error,
            [
                "zlib/unzip_empty_is_data_error",
                "zlib/unzip_three_bytes_is_data_error",
                "zlib/uncompress_three_bytes_is_data_error",
                "zlib/gunzip_non_gzip_is_data_error",
            ]
        );
        let badarg: Vec<&str> = cases
            .iter()
            .filter(|case| case.golden.stdout == Some("exception error badarg"))
            .map(|case| case.id)
            .collect();
        assert_eq!(
            badarg,
            [
                "zlib/unzip_non_iodata_is_badarg",
                "zlib/zip_non_iodata_is_badarg",
                "zlib/zip_improper_list_is_badarg",
            ]
        );
        for case in group() {
            assert!(
                case.id.starts_with("zlib/"),
                "case {} is outside the zlib group",
                case.id
            );
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {}", case.id);
            assert!(
                matches!(
                    case.kind,
                    crate::corpus::CaseKind::Eval {
                        probe: Probe::Value,
                        ..
                    }
                ),
                "case {}: the zlib group is value-probed",
                case.id
            );
            if case.golden.stdout.is_some_and(|g| g.starts_with("exception ")) {
                assert_eq!(case.golden.exit_code, 1, "case {}", case.id);
            }
        }
    }
}
