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

//! Contract **A.7** — the `binary` module's compiled-pattern surface.
//!
//! `binary:compile_pattern/1` returns an *opaque* handle whose tag encodes the search
//! strategy the runtime chose (`{bm,_}` for a one-word pattern, `{ac,_}` for a multi-word
//! one), and `match/2,3` / `matches/2,3` / `split/2,3` dispatch on it. That tag is
//! observable, so the shape is part of the compatibility surface — and the handle itself is
//! a `Ref` whose id varies per VM start, so the cases assert the **shape**
//! (`element(1, P)`, `is_reference(...)`), never the raw handle.
//!
//! The shape rule (oracle-verified, mirrored from the C `maybe_binary_match_compile` via
//! `../../crates/beam_bifs`): exactly one "word" of pattern — a single binary, **or a
//! one-element list of binaries** — compiles to `{bm, Ref}`; more than one word compiles to
//! `{ac, Ref}`. There is no `am` tag. `badarg` covers an empty binary, an empty list, a
//! non-byte-aligned bitstring, an improper list, and any element that is not a byte-aligned
//! binary (including an integer — bytes are not accepted here).
//!
//! Also here: `iolist_size/1` of a size that outgrows the 60-bit small range. That branch is
//! **not expressible through an observable program** and is deliberately not pinned — see
//! the note below.
//!
//! # Deliberately NOT pinned (handoffs to the runtime nodes)
//!
//! Each of these produces a *plain value/reason* (not a capability-gap signal), so a case
//! would be a `mismatch`; they are recorded here instead.
//!
//! * **`iolist_size/1` beyond the 60-bit small range** — unreachable: an iolist's size is the
//!   sum of the *actual* byte lengths of the terms it references, so any size ≥ 2^59 needs at
//!   least 2^59 byte-references (≥ 2^63 bytes of cons cells). The C source says the same
//!   (`erts/emulator/beam/erl_iolist.c:938-948`: "the only way to reach it is to create
//!   a large binary and reference it in absurdum in the same iolist"). The small-range path
//!   agrees (e.g. `iolist_size([<<0>> || _ <- lists:seq(1,10000)])` ⇒ `10000`); a 200 000-
//!   element shared-binary list — still nine orders of magnitude short of the boundary —
//!   already exceeds the Rust evaluator's memory.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `binary` compiled-pattern cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "binary/compile_pattern_bm",
            description: "compile_pattern/1 of a single byte-aligned binary is a {bm, Ref} handle",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<1,2,3>>), {element(1,P), is_reference(element(2,P)), tuple_size(P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {bm,true,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/compile_pattern_ac",
            description: "compile_pattern/1 of a one-element list stays {bm, _}; more than one pattern is {ac, _}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern([<<1,2,3>>]), P2 = binary:compile_pattern([<<1,2,3>>,<<4,5,6>>]), {element(1,P), element(1,P2), is_reference(element(2,P)), is_reference(element(2,P2)), tuple_size(P2)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {bm,ac,true,true,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/compile_pattern_badarg",
            description: "the rejected compile_pattern/1 arguments (empty, non-binary, improper, non-byte-aligned, integer) are badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the **reason** is observed: an uncaught `catch` would carry a stack
                // trace, whose frames are volatile.
                expr: "[begin try binary:compile_pattern(A) catch error:Reason -> Reason end end || A <- [<<>>, [], [<<1>>,65], [<<1,2>>|<<3,4>>], 65, [<<>>], <<1:3>>, [[<<1>>]], [65,66]]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg]"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/match_compiled",
            description: "match/2,3 dispatch on a compiled {bm,_} and {ac,_} handle, including the {scope,_} option",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\"bc\">>), P2 = binary:compile_pattern([<<\"bc\">>,<<\"ca\">>]), {binary:match(<<\"abcabc\">>, P), binary:match(<<\"abcabc\">>, P2), binary:match(<<\"abcabc\">>, P, [{scope,{2,4}}])} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{1,2},{1,2},{4,2}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/match_nomatch",
            description: "the no-match path of match/2 and matches/2 returns nomatch / the empty list, for a raw, a compiled and a pattern-list subject",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:match(<<\"abc\">>, <<\"z\">>), binary:match(<<\"abc\">>, binary:compile_pattern(<<\"z\">>)), binary:match(<<\"abc\">>, [<<\"z\">>]), binary:matches(<<\"abc\">>, <<\"z\">>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {nomatch,nomatch,nomatch,[]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/matches_compiled",
            description: "matches/2 on a compiled pattern yields every non-overlapping match, left to right",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\"bc\">>), P2 = binary:compile_pattern([<<\"bc\">>,<<\"abc\">>]), {binary:matches(<<\"abcabc\">>, P), binary:matches(<<\"abcabc\">>, P2), binary:matches(<<>>, P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{1,2},{4,2}],[{0,3},{3,3}],[]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/split_compiled",
            description: "split/2,3 on a compiled {bm,_}/{ac,_} handle: first-only by default, global/trim via options, subject unchanged when there is no match",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\",\">>), P2 = binary:compile_pattern([<<\",\">>,<<\"b\">>]), {binary:split(<<\"a,b,c\">>, P), binary:split(<<\"a,b,c\">>, P, [global]), binary:split(<<\"a,b,c\">>, P, [global,trim]), binary:split(<<\"a,b,c\">>, P2, [global]), binary:split(<<\"abc\">>, P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>],[<<97,98,99>>]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/split_plain",
            description: "the plain (uncompiled) split/2,3 path agrees with the compiled one for a binary and for a pattern list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:split(<<\"a,b,c\">>, <<\",\">>), binary:split(<<\"a,b,c\">>, <<\",\">>, [global]), binary:split(<<\"a,b,c\">>, [<<\",\">>,<<\"b\">>], [global])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>]}"),
                exit_code: 0,
            },
        },
        // --- the plain `binary` module surface (encoding, slicing, hex, sub-binaries) --------
        Case {
            id: "binary/encode_hex_roundtrip",
            description: "encode_hex/1,2 (upper/lower) and decode_hex/1 round trip byte-for-byte",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:encode_hex(<<1,2,255>>), binary:encode_hex(<<1,2,255>>, lowercase), binary:decode_hex(<<48,49,48,50,102,102>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<48,49,48,50,70,70>>,<<48,49,48,50,102,102>>,<<1,2,255>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/at_first_last",
            description: "binary:at/2, first/1 and last/1 index the documented ends",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:at(<<1,2,3>>, 1), binary:last(<<1,2,9>>), binary:first(<<8,9,10>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,9,8}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/copy_and_bitsize",
            description: "binary:copy/2 repeats a subject (count 0 is empty) and bit_size/1 counts bits",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `bit_size/1` is a BIF that counts **bits** (24 here), not bytes.
                expr: "{binary:copy(<<>>, 5), binary:copy(<<1,2>>, 3), bit_size(<<1,2,3>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<>>,<<1,2,1,2,1,2>>,24}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/part_split_list_to_bin",
            description: "binary:part/3 slicing, list_to_bin/1 and bin_to_list/1,3 agree",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{{binary:part(<<1,2,3,4>>, 0, 2), binary:part(<<1,2,3,4>>, 2, 2)}, binary:list_to_bin([1,2,3]), binary:bin_to_list(<<1,2,3>>), binary:bin_to_list(<<1,2,3>>, 1, 2)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{<<1,2>>,<<3,4>>},<<1,2,3>>,[1,2,3],[2,3]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/longest_common",
            description: "binary:longest_common_prefix/1 and longest_common_suffix/1 agree",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:longest_common_prefix([<<1,2,3>>, <<1,2,4>>]), binary:longest_common_suffix([<<1,2,3>>, <<0,2,3>>])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/sub_binary_patterns",
            description: "the sub-binary match patterns <<B:N/binary, Rest/binary>> agree at two sizes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A sub-binary pattern yields a *reference into* the same binary; the shapes
                // (and the tail split) are what an observable program depends on.
                expr: "{begin <<A:4/binary, B/binary>> = <<1,2,3,4,5,6>>, {A, B} end, begin <<D:2/binary, E/binary>> = <<2,3,4>>, {D, E} end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{<<1,2,3,4>>,<<5,6>>},{<<2,3>>,<<4>>}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/replace_family",
            description: "binary:replace/3,4: first-only by default, [global], a literal with no match, and a pattern list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:replace(<<\"abcabc\">>, <<\"b\">>, <<\"X\">>), binary:replace(<<\"abcabc\">>, <<\"b\">>, <<\"X\">>, [global]), binary:replace(<<\"abc\">>, <<\"z\">>, <<\"X\">>), binary:replace(<<\"abcabc\">>, [<<\"b\">>,<<\"c\">>], <<\"X\">>, [global])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<97,88,99,97,98,99>>,<<97,88,99,97,88,99>>,<<97,98,99>>,<<97,88,88,97,88,88>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/badarg_surface",
            description: "the rejected binary-argument shapes (out-of-range index, over-long part, odd hex, non-byte-aligned, negative count, non-binary subject) are badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the **reason** is observed for each call (a bare `catch` would embed a
                // volatile stack trace).
                expr: "[begin try binary:at(<<1,2>>, 5) catch error:R1 -> R1 end end, begin try binary:part(<<1,2>>, 0, 3) catch error:R2 -> R2 end end, begin try binary:decode_hex(<<48,49,48>>) catch error:R3 -> R3 end end, begin try binary:encode_hex(<<1:3>>) catch error:R4 -> R4 end end, begin try binary:copy(<<1,2>>, -1) catch error:R5 -> R5 end end, begin try binary:at(foo, 0) catch error:R6 -> R6 end end]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [badarg,badarg,badarg,badarg,badarg,badarg]"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.7 compiled-pattern group: every case is a `value` eval case that
    //! observes the `binary` module through a golden the oracle printed, and **no** case ever
    //! renders a per-VM-start handle.
    use super::{group, CaseKind, Contract, Golden, Probe};

    #[test]
    fn every_case_is_a_binary_eval_case_exposing_the_otp_app_contract() {
        for case in group() {
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {}", case.id);
            assert!(
                case.id.starts_with("binary/"),
                "case {} is outside the binary group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {}: an A.7 BIF case must be an eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                extra_argv.is_empty(),
                "case {} takes no extra argv",
                case.id
            );
            assert!(!expr.is_empty() && !expr.contains('\n'), "case {}", case.id);
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
    }

    #[test]
    fn the_compiled_handle_is_always_reduced_to_its_shape() {
        // The handle is a `Ref` whose id varies per VM start, so a golden that rendered it
        // would drift on the next run. Every compile_pattern case therefore projects it:
        // the tag with `element(1, …)` and the id with `is_reference/1`.
        for case in group() {
            let Some(golden) = case.golden.stdout else {
                panic!("case {} pins no golden", case.id);
            };
            assert!(
                !golden.contains("#Ref<"),
                "case {}: a raw handle leaked into the golden {golden:?}",
                case.id
            );
        }
        for id in ["binary/compile_pattern_bm", "binary/compile_pattern_ac"] {
            let case = group()
                .into_iter()
                .find(|c| c.id == id)
                .expect("registered");
            let expr = case.expr().expect("eval case");
            assert!(
                expr.contains("element(1,") && expr.contains("is_reference(element(2,"),
                "case {id}: the handle must be observed by shape, got {expr}"
            );
        }
    }

    #[test]
    fn the_group_pins_the_documented_tag_rule_and_badarg_set() {
        for (id, stdout) in [
            ("binary/compile_pattern_bm", "ok {bm,true,2}"),
            ("binary/compile_pattern_ac", "ok {bm,ac,true,true,2}"),
            (
                "binary/compile_pattern_badarg",
                "ok [badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg]",
            ),
            ("binary/match_compiled", "ok {{1,2},{1,2},{4,2}}"),
            (
                "binary/matches_compiled",
                "ok {[{1,2},{4,2}],[{0,3},{3,3}],[]}",
            ),
            (
                "binary/split_compiled",
                "ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>],[<<97,98,99>>]}",
            ),
            (
                "binary/split_plain",
                "ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>]}",
            ),
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code: 0
                },
                "{id}"
            );
        }
        // The `badarg` case really enumerates the rejected shapes: an empty binary, an empty
        // list, a non-binary element, an improper list, a bare integer, an empty element, a
        // non-byte-aligned bitstring, a nested list and a list of integers.
        let badarg = group()
            .into_iter()
            .find(|c| c.id == "binary/compile_pattern_badarg")
            .expect("registered");
        let expr = badarg.expr().expect("eval case");
        for rejected in [
            "<<>>",
            "[]",
            "[<<1>>,65]",
            "[<<1,2>>|<<3,4>>]",
            "65",
            "[<<>>]",
            "<<1:3>>",
            "[[<<1>>]]",
            "[65,66]",
        ] {
            assert!(
                expr.contains(rejected),
                "the badarg case no longer rejects {rejected:?}"
            );
        }
    }

    #[test]
    fn the_no_match_paths_are_pinned() {
        // Every no-match shape agrees with the oracle, so each is pinned directly: `nomatch`
        // for `match/2` (raw, compiled and pattern-list subjects) and the empty list for
        // `matches/2` on a compiled pattern and for `split/2` (subject unchanged).
        let cases = group();
        let nomatch = cases
            .iter()
            .find(|c| c.id == "binary/match_nomatch")
            .expect("registered");
        assert_eq!(
            nomatch.golden,
            Golden {
                stdout: Some("ok {nomatch,nomatch,nomatch,[]}"),
                exit_code: 0
            }
        );
        let matches = cases
            .iter()
            .find(|c| c.id == "binary/matches_compiled")
            .expect("registered");
        assert!(matches.golden.stdout.is_some_and(|g| g.ends_with(",[]}")));
        let split = cases
            .iter()
            .find(|c| c.id == "binary/split_compiled")
            .expect("registered");
        assert!(split
            .golden
            .stdout
            .is_some_and(|g| g.ends_with("[<<97,98,99>>]}")));
    }
}
