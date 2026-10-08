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

//! Contract **A.7** — the two externally-observable legacy hashes.
//!
//! `erlang:phash/2` (the C `make_hash`) and `erlang:phash2/1,2` (the C `make_hash2`) are
//! *frozen* observable behaviour: a program that persisted a hash bucket or a term-hash key
//! must read the same value from the rewrite. They are their own group because the pinned
//! values come from bit-exact transcriptions of `erl_term_hashing.c` (`../../crates/beam_bifs`
//! `src/phash.rs`), not from the term API — so the goldens are the strongest possible
//! statement that the transcription is right.
//!
//! Grouped by the input family that exercises a distinct branch of the C:
//! small-int boundaries, the block-hash binary path (by length), a non-byte-aligned
//! bitstring, a map (which routes through the *other* hash), `NIL`/improper lists, and the
//! `Range` argument.
//!
//! The observable is always a tuple of **integers**, rendered with the *value* probe (`~w`),
//! so nothing here depends on a per-VM-start value: no pid/ref/port, no multi-key map. The
//! single-key maps are safe because `phash2/1` of a map hashes its key/value pairs in
//! term order, never a `~w` rendering of the map itself.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `phash`/`phash2` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "phash2/int_boundaries",
            description: "phash2/1 across the small-int sign/magnitude byte-scan boundaries (2^27-1, 2^27 and their negatives)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(134217727), erlang:phash2(134217728), \
                       erlang:phash2(-134217727), erlang:phash2(-134217728)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {112602999,12354923,96386750,69672967}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/float_negative_zero",
            description: "phash2/1 and phash/2 of 0.0 / -0.0 (the legacy hash normalises the sign) plus a plain float",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(0.0), erlang:phash2(-0.0), erlang:phash(0.0, 1000000), \
                       erlang:phash(-0.0, 1000000), erlang:phash2(1.5)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {20875736,20875736,1,1,10380315}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/atom_nil_tuple_improper_list",
            description: "phash2/1 of NIL, an atom, a tuple and an improper list (the terminating non-list tail step)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2([]), erlang:phash2(hello), erlang:phash2({1,2,3}), \
                       erlang:phash2([1|2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {113427502,7258927,28734975,86124794}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/binary_lengths_0_64_and_large",
            description: "phash2/1 by binary length: the empty and a short binary plus the >=12-byte block-hash path (64/200 bytes)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<>>), erlang:phash2(<<1,2,3>>), \
                       erlang:phash2(binary:copy(<<1>>, 64)), erlang:phash2(binary:copy(<<7>>, 200))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {13708901,6479071,72330118,121929617}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/non_byte_aligned_bitstring",
            description: "phash2/1 of bitstrings whose bit size is not a multiple of 8 (the partial final-byte step)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<1:1>>), erlang:phash2(<<1:3,2:5>>), erlang:phash2(<<255:3>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {102233125,97170390,63553086}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/map_and_external_fun",
            description: "phash2/1 of a single-key map (which routes through the other hash) and of an external fun",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(#{a => 1}), erlang:phash2(fun lists:reverse/1)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {59617982,134027972}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/range_0_1_and_2pow32_minus_1",
            description: "phash2/2's Range argument: 1, 2^32-1, and 0 (which is badarg)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<1,2,3>>, 1), erlang:phash2(<<1,2,3>>, 4294967295), \
                       try erlang:phash2(<<1,2,3>>, 0) catch error:Reason -> Reason end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {0,4167228639,badarg}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash/legacy_map_and_fun",
            description: "phash/2 (the legacy make_hash) of a single-key map, an external fun, a small int and a binary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash(#{a => 1}, 1000000), erlang:phash(fun lists:reverse/1, 1000000), \
                       erlang:phash(1, 4), erlang:phash(<<1,2,3>>, 1000000)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {235874,832296,4,692590}"),
                exit_code: 0,
            },
        },
    ]
}
