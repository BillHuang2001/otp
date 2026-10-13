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

//! Contract **A.7** — `erlang:decode_packet/3` across the packet types.
//!
//! `decode_packet/3` selects its wire format with a *Type* argument and answers
//! `{ok, Packet, Rest}`, `{more, Length | undefined}` or `{error, invalid}`. The BIF accepts
//! exactly four integer forms and twelve named atoms, where `0` is `raw`:
//!
//! | Type | wire format |
//! |---|---|
//! | `0` / `raw` | no framing: the whole buffer is one packet |
//! | `1` | one-byte length prefix |
//! | `2` | two-byte big-endian length prefix |
//! | `4` | four-byte big-endian length prefix |
//! | `asn1` | ASN.1 BER (short and long length forms) |
//! | `sunrm` | Sun RPC: four-byte big-endian length, header kept in the packet |
//! | `cdr` | GIOP/CDR (12-byte header, big- or little-endian message size) |
//! | `fcgi` | FastCGI record |
//! | `line` | up to and including the line delimiter |
//! | `tpkt` | TPKT frame |
//! | `ssl_tls` | TLS record |
//! | `http` / `httph` | HTTP request/status line and header lines (text) |
//! | `http_bin` / `httph_bin` | the same, decoded into binaries |
//!
//! This group gives every accepted type a **complete-packet** case and, where the BIF expresses
//! them, a truncated (`{more, N}` / `{more, undefined}`) and an invalid (`{error, invalid}` /
//! `error:badarg`) shape. Every golden is the oracle's output, so a case is present only when the
//! oracle and the Rust runtime agree on the bytes. `ftp` is *not* a packet type (it is
//! `error:badarg` on both runtimes) and is pinned as such.

use super::{Case, CaseKind, Contract, Golden, Probe};

pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- raw (the integer form 0) ---------------------------------------------------------
        Case {
            id: "decode_packet/raw_returns_the_whole_buffer",
            description: "the raw packet type returns the whole buffer with an empty rest",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(raw, <<\"hello\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,5,104,101,108,108,111,109,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/integer_zero_is_the_raw_packet_type",
            description: "packet type 0 is the raw packet type",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(0, <<\"hello\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,5,104,101,108,108,111,109,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/raw_empty_buffer_is_more_undefined",
            description: "raw of an empty buffer reports {more,undefined}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(raw, <<>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        // --- the integer length-prefix forms 1, 2 and 4 ---------------------------------------
        Case {
            id: "decode_packet/one_byte_length_prefix",
            description: "packet type 1 strips a one-byte length prefix",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(1, <<3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/one_byte_length_prefix_truncated",
            description: "packet type 1 with a short body reports {more,4}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(1, <<3,97,98>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,97,4>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/two_byte_length_prefix",
            description: "packet type 2 strips a two-byte big-endian length prefix",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(2, <<0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/two_byte_length_prefix_truncated",
            description: "packet type 2 with a short body reports {more,5}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(2, <<0,3,97>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,97,5>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/four_byte_length_prefix",
            description: "packet type 4 strips a four-byte big-endian length prefix",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(4, <<0,0,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/four_byte_length_prefix_truncated",
            description: "packet type 4 with a short body reports {more,7}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(4, <<0,0,0,3,97>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,97,7>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/sunrm_keeps_the_length_header",
            description: "the sunrm packet type keeps the four-byte length header in the packet",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(sunrm, <<0,0,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,7,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        // --- line ------------------------------------------------------------------------------
        Case {
            id: "decode_packet/line_returns_through_the_delimiter",
            description: "the line packet type returns the line through the line feed",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(line, <<\"abc\\r\\nxyz\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,5,97,98,99,13,10,109,0,0,0,3,120,121,122>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/line_without_a_delimiter_is_more_undefined",
            description: "the line packet type without a delimiter reports {more,undefined}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(line, <<\"abc\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/line_with_a_line_length_option_truncates",
            description: "the line packet type with {line_length,4} truncates the line",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(line, <<\"abc\\r\\nxyz\">>, [{line_length, 4}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,4,97,98,99,13,109,0,0,0,4,10,120,121,122>>"),
                exit_code: 0,
            },
        },
        // --- asn1 ------------------------------------------------------------------------------
        Case {
            id: "decode_packet/asn1_short_length_form",
            description: "the asn1 packet type decodes a short-form length",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(asn1, <<2,3,1,2,3,99>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,5,2,3,1,2,3,109,0,0,0,1,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/asn1_long_length_form",
            description: "the asn1 packet type decodes a long-form (0x82) length",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(asn1, <<2,130,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,7,2,130,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/asn1_truncated",
            description: "the asn1 packet type with a short body reports {more,5}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(asn1, <<2,3,1>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,97,5>>"),
                exit_code: 0,
            },
        },
        // --- cdr -------------------------------------------------------------------------------
        Case {
            id: "decode_packet/cdr_big_endian_message",
            description: "the cdr packet type decodes a big-endian GIOP message",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(cdr, <<\"GIOP\",1,2,0,0,0,0,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,15,71,73,79,80,1,2,0,0,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/cdr_little_endian_message",
            description: "the cdr packet type honours the little-endian GIOP flag",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(cdr, <<\"GIOP\",1,2,1,0,3,0,0,0,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,15,71,73,79,80,1,2,1,0,3,0,0,0,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/cdr_bad_magic_is_invalid",
            description: "the cdr packet type rejects a message without the GIOP magic",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(cdr, <<\"XXXX\",1,2,0,0,0,0,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,119,7,105,110,118,97,108,105,100>>"),
                exit_code: 0,
            },
        },
        // --- fcgi ------------------------------------------------------------------------------
        Case {
            id: "decode_packet/fcgi_record",
            description: "the fcgi packet type decodes a FastCGI record",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(fcgi, <<1,1,0,1,0,3,0,0,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,11,1,1,0,1,0,3,0,0,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/fcgi_bad_version_is_invalid",
            description: "the fcgi packet type rejects a record with a bad version",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(fcgi, <<2,1,0,1,0,3,0,0,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,119,7,105,110,118,97,108,105,100>>"),
                exit_code: 0,
            },
        },
        // --- tpkt ------------------------------------------------------------------------------
        Case {
            id: "decode_packet/tpkt_packet",
            description: "the tpkt packet type decodes a TPKT frame",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(tpkt, <<3,0,0,7,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,109,0,0,0,7,3,0,0,7,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/tpkt_bad_version_is_invalid",
            description: "the tpkt packet type rejects a frame with a bad version",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(tpkt, <<4,0,0,7,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,119,7,105,110,118,97,108,105,100>>"),
                exit_code: 0,
            },
        },
        // --- ssl_tls ---------------------------------------------------------------------------
        Case {
            id: "decode_packet/ssl_tls_record",
            description: "the ssl_tls packet type decodes a TLS record",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(ssl_tls, <<22,3,1,0,3,97,98,99,88>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,5,119,7,115,115,108,95,116,108,115,106,97,22,104,2,97,3,97,1,109,0,0,0,3,97,98,99,109,0,0,0,1,88>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/ssl_tls_truncated_is_more_undefined",
            description: "the ssl_tls packet type with a short record reports {more,undefined}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(ssl_tls, <<22,3,1>>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        // --- http / httph (text form) ----------------------------------------------------------
        Case {
            id: "decode_packet/http_request_line",
            description: "the http packet type decodes a request line",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(http, <<\"GET / HTTP/1.0\\r\\n\\r\\n\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,4,119,12,104,116,116,112,95,114,101,113,117,101,115,116,119,3,71,69,84,104,2,119,8,97,98,115,95,112,97,116,104,107,0,1,47,104,2,97,1,97,0,109,0,0,0,2,13,10>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/http_response_line",
            description: "the http packet type decodes a status line",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(http, <<\"HTTP/1.1 200 OK\\r\\n\\r\\n\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,4,119,13,104,116,116,112,95,114,101,115,112,111,110,115,101,104,2,97,1,97,1,97,200,107,0,2,79,75,109,0,0,0,2,13,10>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/httph_header_line",
            description: "the httph packet type decodes one header line",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(httph, <<\"Content-Length: 5\\r\\n\\r\\nbody\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,5,119,11,104,116,116,112,95,104,101,97,100,101,114,97,38,119,14,67,111,110,116,101,110,116,45,76,101,110,103,116,104,107,0,14,67,111,110,116,101,110,116,45,76,101,110,103,116,104,107,0,1,53,109,0,0,0,6,13,10,98,111,100,121>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/httph_before_the_end_of_headers_is_more_undefined",
            description: "the httph packet type before the end of headers reports {more,undefined}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(httph, <<\"Content-Length: 5\\r\\n\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,109,111,114,101,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        // --- http_bin / httph_bin (binary form) ------------------------------------------------
        Case {
            id: "decode_packet/http_bin_request_line",
            description: "the http_bin packet type decodes a request line into binaries",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(http_bin, <<\"GET / HTTP/1.0\\r\\n\\r\\n\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,4,119,12,104,116,116,112,95,114,101,113,117,101,115,116,119,3,71,69,84,104,2,119,8,97,98,115,95,112,97,116,104,109,0,0,0,1,47,104,2,97,1,97,0,109,0,0,0,2,13,10>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "decode_packet/httph_bin_header_line",
            description: "the httph_bin packet type decodes one header line into binaries",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(httph_bin, <<\"Content-Length: 5\\r\\n\\r\\nbody\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,3,119,2,111,107,104,5,119,11,104,116,116,112,95,104,101,97,100,101,114,97,38,119,14,67,111,110,116,101,110,116,45,76,101,110,103,116,104,109,0,0,0,14,67,111,110,116,101,110,116,45,76,101,110,103,116,104,109,0,0,0,1,53,109,0,0,0,6,13,10,98,111,100,121>>"),
                exit_code: 0,
            },
        },
        // --- the rejected Type / argument shapes -----------------------------------------------
        Case {
            id: "decode_packet/integer_three_is_not_a_packet_type",
            description: "packet type 3 is not a valid type (error:badarg)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(3, <<\"x\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "decode_packet/integer_five_is_not_a_packet_type",
            description: "packet type 5 is not a valid type (error:badarg)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(5, <<\"x\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "decode_packet/unknown_type_atom_is_badarg",
            description: "an unknown packet-type atom is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(bogus, <<\"x\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "decode_packet/ftp_is_not_a_packet_type",
            description: "the ftp atom is not a packet type (error:badarg)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(ftp, <<\"ABOR\\r\\n\">>, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "decode_packet/non_binary_data_is_badarg",
            description: "a non-binary data argument is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(raw, \"hello\", [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "decode_packet/non_list_options_is_badarg",
            description: "a non-list option argument is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:decode_packet(raw, <<\"x\">>, notalist)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    #[test]
    fn the_group_covers_every_accepted_packet_type() {
        let cases = group();
        assert_eq!(cases.len(), 37, "the A.7 decode_packet group has thirty-seven cases");
        for case in &cases {
            assert!(case.id.starts_with("decode_packet/"), "{}", case.id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "{}", case.id);
            assert!(
                case.expr()
                    .is_some_and(|expr| expr.contains("erlang:decode_packet(")),
                "{}",
                case.id
            );
            assert!(case.extra_argv().is_empty(), "{}", case.id);
            assert!(case.env().is_empty(), "{}", case.id);
            assert_eq!(case.kind_label(), Probe::TermToBinary.as_str(), "{}", case.id);
            let CaseKind::Eval { probe, .. } = &case.kind else {
                panic!("{} must be an Eval case", case.id);
            };
            assert_eq!(*probe, Probe::TermToBinary, "{}", case.id);
        }
        // Exactly the packet types the BIF accepts: the integer forms 0/1/2/4 plus the named
        // types raw, asn1, sunrm, cdr, fcgi, line, tpkt, ssl_tls, http, httph, http_bin and
        // httph_bin — one complete-packet case each.
        for needle in [
            "decode_packet(raw,",
            "decode_packet(0,",
            "decode_packet(1,",
            "decode_packet(2,",
            "decode_packet(4,",
            "decode_packet(sunrm,",
            "decode_packet(line,",
            "decode_packet(asn1,",
            "decode_packet(cdr,",
            "decode_packet(fcgi,",
            "decode_packet(tpkt,",
            "decode_packet(ssl_tls,",
            "decode_packet(http,",
            "decode_packet(httph,",
            "decode_packet(http_bin,",
            "decode_packet(httph_bin,",
        ] {
            assert!(
                cases
                    .iter()
                    .any(|case| case.expr().is_some_and(|expr| expr.contains(needle))),
                "no case exercises {needle}"
            );
        }
    }

    #[test]
    fn the_more_shapes_pin_the_expected_remaining_lengths() {
        // A short body leaves the decoder needing exactly `hlen + plen` bytes.
        for (id, stdout) in [
            (
                "decode_packet/one_byte_length_prefix_truncated",
                "ok <<131,104,2,119,4,109,111,114,101,97,4>>",
            ),
            (
                "decode_packet/two_byte_length_prefix_truncated",
                "ok <<131,104,2,119,4,109,111,114,101,97,5>>",
            ),
            (
                "decode_packet/four_byte_length_prefix_truncated",
                "ok <<131,104,2,119,4,109,111,114,101,97,7>>",
            ),
            (
                "decode_packet/asn1_truncated",
                "ok <<131,104,2,119,4,109,111,114,101,97,5>>",
            ),
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some(stdout), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
        // The raw / delimiter / HTTP-family / TLS decoders report an *unknown* length.
        const MORE_UNDEFINED: &str = "ok <<131,104,2,119,4,109,111,114,101,119,9,117,110,100,101,102,105,110,101,100>>";
        for id in [
            "decode_packet/raw_empty_buffer_is_more_undefined",
            "decode_packet/line_without_a_delimiter_is_more_undefined",
            "decode_packet/ssl_tls_truncated_is_more_undefined",
            "decode_packet/httph_before_the_end_of_headers_is_more_undefined",
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some(MORE_UNDEFINED), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
    }

    #[test]
    fn the_invalid_frames_and_rejected_argument_shapes_are_pinned() {
        const INVALID: &str = "ok <<131,104,2,119,5,101,114,114,111,114,119,7,105,110,118,97,108,105,100>>";
        for id in [
            "decode_packet/cdr_bad_magic_is_invalid",
            "decode_packet/fcgi_bad_version_is_invalid",
            "decode_packet/tpkt_bad_version_is_invalid",
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some(INVALID), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
        // A rejected Type (an unknown integer or atom, including `ftp`) or a malformed argument
        // (non-binary data, a non-list option) is `error:badarg` on both runtimes.
        let badarg: Vec<&str> = group()
            .iter()
            .filter(|case| case.golden.stdout == Some("exception error badarg"))
            .map(|case| case.id)
            .collect();
        assert_eq!(
            badarg,
            [
                "decode_packet/integer_three_is_not_a_packet_type",
                "decode_packet/integer_five_is_not_a_packet_type",
                "decode_packet/unknown_type_atom_is_badarg",
                "decode_packet/ftp_is_not_a_packet_type",
                "decode_packet/non_binary_data_is_badarg",
                "decode_packet/non_list_options_is_badarg",
            ]
        );
        for case in group() {
            if case.golden.stdout == Some("exception error badarg") {
                assert_eq!(case.golden.exit_code, 1, "{}", case.id);
            }
        }
    }
}
