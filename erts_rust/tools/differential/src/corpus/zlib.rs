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
//! Contract **A.7** — the `zlib` **raw-DEFLATE** pair, `zlib:zip/1` / `zlib:unzip/1`.
//!
//! `zlib:zip/1` emits a header-less, checksum-less DEFLATE stream (RFC 1951) — the bytes of
//! which are *the encoder's choice*, not a codec contract — so what this group pins is:
//!
//! * **byte-exact goldens for tiny bodies.** `<<"hello">>` (`<<203,72,205,201,201,7,0>>`,
//!   a fixed-Huffman block), `<<>>` (`<<3,0>>`), the stored block `<<3,4,5>>`
//!   (`<<99,102,97,5,0>>`) and the iodata list `[1,2,3]` (`<<99,100,98,6,0>>`) are the four
//!   bodies the sibling crate's live-oracle check (`crates/beam_bifs/tests/zlib.rs`) resolves
//!   to the *same* bytes on both runtimes, so they are stable goldens rather than assumptions.
//!   Every golden here is the **oracle's** own output, re-verified with `differential run <id>`;
//!   a body whose image is *not* reproduced byte for byte is deliberately absent (see below).
//! * **the round trip.** `zlib:unzip(zlib:zip(B)) =:= B` over an empty, a tiny and a
//!   100-byte body — the inflater is a hand-written decoder, and the round trip is its
//!   correctness evidence whatever block type the encoder picked. Note that only the *round
//!   trip* is pinned for the larger bodies: the **compressed image** of a body is the
//!   encoder's own choice and, for the 100-byte body, genuinely **differs** between the two
//!   runtimes (the oracle's stream is 102 bytes, the Rust runtime's 105 — both valid DEFLATE,
//!   but a different block choice), so the 100-byte body carries a byte-exact
//!   *round-trip identity* golden, not a `byte_size/1` shape golden (a shape case there would
//!   be a `mismatch`). Only the four bodies above carry a byte-exact golden.
//! * **the error matrix.** `zlib:unzip/1` has **no length arm**: an empty stream and a
//!   three-byte stream are both the bare `error:data_error` (never `badarg`), while a
//!   *non-iodata* argument (`42`) is `error:badarg` on both `zip/1` and `unzip/1` — and so is
//!   an improper list (`[1,2|3]`), whose `zip/1` would have to walk a non-list tail.
//!
//! The **zlib-wrapped** siblings `zlib:compress/1` / `zlib:uncompress/1` (a 2-byte zlib header
//! + Adler-32) and `zlib:gzip/1` / `zlib:gunzip/1` (the gzip container) are pinned too:
//! byte-exact tiny-body goldens (`<<"abc">>`), the encoder/inflater round trips, and the bare
//! `error:data_error` failure class — a non-stream or non-gzip argument carries no length arm
//! and raises `data_error`, never `badarg`.
//!
//! Not pinned here (and why):
//!
//! * The compressed **image** of a larger or repetitive body: the encoder is free to choose
//!   stored, fixed or dynamic Huffman blocks, so `zlib:compress(<<0:8192>>)` resolves to a
//!   different — but equally valid — stream than the oracle's 17 bytes. Only bodies whose
//!   image the oracle reproduces byte for byte carry a byte-exact golden.
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
                // The `=:=` compares the *body*, so the case stays valid whatever block type
                // the encoder chose (see the module doc).
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
                // A tiny body, so the encoder's block choice is stable across the runtimes —
                // pinned byte-exactly (see the module doc for why a larger body is not).
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
            description: "zlib:uncompress/1 inverts zlib:compress/1 for a 100-byte body (the \
                          compressed image itself is the encoder's choice)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `=:=` compares the *body*, so the case stays valid whatever block type
                // the encoder chose (its compressed image is not pinned byte-exactly).
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

    /// The round-trip family: the inflater inverts the encoder for every body, including the
    /// one body whose compressed image is *not* pinned byte-exactly.
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
        // The 100-byte body is pinned by the identity only: its compressed *size* differs
        // between the runtimes (the oracle's stream is 102 bytes, the Rust runtime's 105), so
        // a `byte_size/1` shape case would be a mismatch. See the module doc.
        assert_eq!(
            find("zlib/unzip_zip_roundtrip_100_bytes_is_identity").expr(),
            Some(
                "zlib:unzip(zlib:zip(list_to_binary(lists:seq(1,100)))) \
                 =:= list_to_binary(lists:seq(1,100))"
            )
        );
        assert!(
            !group().iter().any(|case| case.expr().is_some_and(|e| e.contains("byte_size"))),
            "no `byte_size/1` shape case may be pinned: the encoder's block choice is not stable"
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
