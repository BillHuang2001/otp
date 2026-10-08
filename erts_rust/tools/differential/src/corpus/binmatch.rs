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
//! * **The `nomatch` result of `binary:match/2,3`** — the oracle prints `nomatch` while the
//!   Rust runtime fails the whole evaluation with `beam_rt: term not found` (empty stdout,
//!   exit `1`): the `nomatch` atom is not interned on the `binary:match` bridge. It is not
//!   specific to the compiled path — the raw-subject form fails identically — and the bare
//!   atom `nomatch` evaluates fine, so the gap is in the BIF's result construction.
//!   `binary:matches/2` (empty list) and `binary:split/2,3` (subject unchanged) *do* agree on
//!   their no-match paths and are pinned below.
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
    fn the_no_match_paths_are_pinned_where_they_are_expressible() {
        // `binary:match/2`'s `nomatch` result diverges today (see the module docs) and is
        // therefore absent; the two agreeing no-match shapes must stay pinned.
        let cases = group();
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
        for case in &cases {
            let expr = case.expr().expect("eval case");
            assert!(
                !expr.contains("<<\"zz\">>") && !expr.contains("nomatch"),
                "case {}: the diverging nomatch path must not be pinned",
                case.id
            );
        }
    }
}
