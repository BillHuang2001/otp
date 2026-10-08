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

//! Contract **A.7** — the `~p` printer and the `short` float rendering.
//!
//! Two renderings a program's output depends on and that a runtime must reproduce
//! byte-for-byte:
//!
//! * the `~p`/`~P` printer (the `io_lib_pretty` port): nested containers, re-flow at the
//!   line limit, the explicit depth argument of `~P`/`~W`, and the printable-latin1 rule
//!   that decides whether a list/binary renders as a string;
//! * `float_to_list/2` / `float_to_binary/2` with the `short` option (the C
//!   `sys_double_to_chars_short` port), whose notation choice (`0.0001` vs `1.0e-5`) and
//!   exact-tie rule are observable.
//!
//! The probes print a **flattened** rendering (`lists:flatten/1` of `io_lib:format/2`), so
//! the compared stdout line is the exact character sequence a program would see.
//!
//! # `~W` of a binary is only depth-elided at depth 1
//! `~W` is a *flat* depth renderer (`io_lib_format:control_limited/6`), not the
//! `io_lib_pretty` one `~P` goes through — and [`crate`]'s sibling runtime
//! (`crates/beam_bifs` `src/print.rs`) documents that its `~W` of a binary is *not*
//! depth-elided past depth 1 (`render_binary_chunk` only handles `depth == 1`). That is a
//! **real divergence** (the oracle prints `<<1,...>>` at depth 2, the Rust runtime
//! `<<1,2,3>>`), so the `print/w_binary_depth_elision` case pins only the depths that
//! agree: `~W` at depths `0`/`1` (plus a depth past the element count, which renders in
//! full) and `~P` at every depth (its `io_lib_pretty` re-flow *does* elide). The depths
//! `2`/`3` under `~W` are deliberately **not** pinned — see the group's tests.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 printer / float-formatting cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "float/short_to_list",
            description: "float_to_list/2 with [short] over the notation + tie-rule boundary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten([erlang:float_to_list(F, [short]) || F <- [1.0, 1000.0, 1.0e10, 1.0e-4, 1.0e-5, 5.960464477539063e-8]])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [49,46,48,49,46,48,101,51,49,46,48,101,49,48,48,46,48,48,48,49,49,46,48,101,45,53,53,46,57,54,48,52,54,52,52,55,55,53,51,57,48,54,51,101,45,56]"),
                exit_code: 0,
            },
        },
        Case {
            id: "float/short_to_binary",
            description: "float_to_binary/2 with [short] over the same notation boundary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten([erlang:float_to_binary(F, [short]) || F <- [1.0, 1000.0, 1.0e10, 1.0e-4, 1.0e-5, 5.960464477539063e-8]])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [<<49,46,48>>,<<49,46,48,101,51>>,<<49,46,48,101,49,48>>,<<48,46,48,48,48,49>>,<<49,46,48,101,45,53>>,<<53,46,57,54,48,52,54,52,52,55,55,53,51,57,48,54,51,101,45,56>>]"),
                exit_code: 0,
            },
        },
        Case {
            id: "print/p_nested_containers",
            description: "~p of nested tuples/lists/single-key maps/binaries, incl. a >80-column re-flow",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~p~n~p\", [{a,[1,2,3],#{k => [<<\"ab\">>, {x, 1.0}]}, <<1,2,3>>}, [{alpha,1},{beta,2},{gamma,3},{delta,4},{epsilon,5},{zeta,6},{eta,7},{theta,8},{iota,9},{kappa,10}]]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [123,97,44,91,49,44,50,44,51,93,44,35,123,107,32,61,62,32,91,60,60,34,97,98,34,62,62,44,123,120,44,49,46,48,125,93,125,44,60,60,49,44,50,44,51,62,62,125,10,91,123,97,108,112,104,97,44,49,125,44,10,32,123,98,101,116,97,44,50,125,44,10,32,123,103,97,109,109,97,44,51,125,44,10,32,123,100,101,108,116,97,44,52,125,44,10,32,123,101,112,115,105,108,111,110,44,53,125,44,10,32,123,122,101,116,97,44,54,125,44,10,32,123,101,116,97,44,55,125,44,10,32,123,116,104,101,116,97,44,56,125,44,10,32,123,105,111,116,97,44,57,125,44,10,32,123,107,97,112,112,97,44,49,48,125,93]"),
                exit_code: 0,
            },
        },
        Case {
            id: "print/p_depth_limited",
            description: "~P with depth 0/1/2 of a nested structure (`...` elision per depth)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten([io_lib:format(\"~P\", [T, D]) || D <- [0,1,2], T <- [{a,[1,[2,[3,4]]],{b,{c,{d,1}}}}]])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46,123,46,46,46,125,123,97,44,46,46,46,125]"),
                exit_code: 0,
            },
        },
        Case {
            id: "print/p_printable_latin1_binary",
            description: "~p printable-latin1 rule: a printable binary/list renders quoted (+ escapes), a non-printable one as <<...>>",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~p|~p|~p|~p|~p\", [<<\"hello\">>, <<0,1,255>>, [104,101,108,108,111], <<34,92>>, [160,161,255]]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [60,60,34,104,101,108,108,111,34,62,62,124,60,60,48,44,49,44,50,53,53,62,62,124,34,104,101,108,108,111,34,124,60,60,34,92,34,92,92,34,62,62,124,34,160,161,255,34]"),
                exit_code: 0,
            },
        },
        Case {
            id: "print/left_adjusted_badarg",
            description: "a left-adjusted ~-<n>p is badarg (the reason is the observable, not a stack trace)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try lists:flatten(io_lib:format(\"~-10p\", [[1,2]])) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        Case {
            id: "print/w_binary_depth_elision",
            description: "~W/~P of a non-printable binary at increasing depths (only the agreeing depths are pinned)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten([io_lib:format(\"~W\", [<<1,2,3>>, 0]), io_lib:format(\"~W\", [<<1,2,3>>, 1]), io_lib:format(\"~W\", [<<1,2,3>>, 4]), io_lib:format(\"~P\", [<<1,2,3>>, 0]), io_lib:format(\"~P\", [<<1,2,3>>, 1]), io_lib:format(\"~P\", [<<1,2,3>>, 2]), io_lib:format(\"~P\", [<<1,2,3>>, 3]), io_lib:format(\"~P\", [<<1,2,3>>, 4])])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46,60,60,46,46,46,62,62,60,60,49,44,50,44,51,62,62,46,46,46,60,60,46,46,46,62,62,60,60,49,44,46,46,46,62,62,60,60,49,44,50,44,46,46,46,62,62,60,60,49,44,50,44,51,62,62]"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    /// Every case id in this group (the group's contract is pinned by the registry tests
    /// too, but this keeps the ids in one place for the assertions below).
    const IDS: [&str; 7] = [
        "float/short_to_list",
        "float/short_to_binary",
        "print/p_nested_containers",
        "print/p_depth_limited",
        "print/p_printable_latin1_binary",
        "print/left_adjusted_badarg",
        "print/w_binary_depth_elision",
    ];

    fn golden(id: &str) -> &'static str {
        let case = group()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no case {id}"));
        case.golden.stdout.expect("a golden stdout")
    }

    fn expr(id: &str) -> &'static str {
        let case = group()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no case {id}"));
        case.expr().expect("an eval expression")
    }

    #[test]
    fn every_case_is_an_otp_apps_value_probe() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len());
        for id in IDS {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"));
            assert_eq!(case.contracts, &[Contract::OtpApps], "{id}");
            // A `~w` render of the flattened string is the compared observable.
            assert_eq!(case.kind_label(), "value", "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
            assert!(
                case.golden.stdout.is_some_and(|s| !s.is_empty()),
                "{id}: empty golden"
            );
            assert!(
                matches!(
                    case.kind,
                    CaseKind::Eval {
                        probe: Probe::Value,
                        ..
                    }
                ),
                "{id}: expected the value probe"
            );
        }
    }

    #[test]
    fn the_short_float_goldens_span_the_notation_and_tie_rule() {
        // `1.0` / `1.0e3` / `1.0e10` / `0.0001` / `1.0e-5` / `5.960464477539063e-8`:
        // the first two entries prove the notation rule (`1000.0` => `1.0e3`) and the
        // `1.0e-4`/`1.0e-5` pair the `f`-vs-`e` boundary (`0.0001` vs `1.0e-5`).
        let list = golden("float/short_to_list");
        assert!(list.starts_with("ok [49,46,48,49,46,48,101,51"), "{list}");
        assert!(
            list.contains("48,46,48,48,48,49,49,46,48,101,45,53"),
            "{list}"
        );
        assert!(
            list.contains("53,46,57,54,48,52,54,52,52,55,55,53,51,57,48,54,51,101,45,56"),
            "{list}"
        );
        // `float_to_binary/2` keeps the six renderings as six separate binaries.
        let binary = golden("float/short_to_binary");
        assert!(
            binary.starts_with("ok [<<49,46,48>>,<<49,46,48,101,51>>"),
            "{binary}"
        );
        assert!(binary.contains("<<48,46,48,48,48,49>>"), "{binary}");
    }

    #[test]
    fn the_printer_goldens_pin_the_nesting_replay_and_the_badarg_reason() {
        // The nested container renders flat (no `,10,` = newline) but the >80-column list
        // re-flows: the rendering carries an actual newline between the two `~p` chunks.
        let nested = golden("print/p_nested_containers");
        assert!(
            nested.contains(",10,"),
            "the re-flow must emit a newline: {nested}"
        );
        assert!(
            nested.contains("35,123,107,32,61,62,32"),
            "the single-key map must render in the flatmap form: {nested}"
        );
        // `~P` depths 0/1/2 => `...` / `{...}` / `{a,...}`.
        assert_eq!(
            golden("print/p_depth_limited"),
            "ok [46,46,46,123,46,46,46,125,123,97,44,46,46,46,125]"
        );
        // Printable-latin1 rule: `<<"hello">>` and `[104,…]` render quoted, a binary with a
        // non-printable byte (255) renders as `<<0,1,255>>`, and `"`/`\` are escaped.
        let latin1 = golden("print/p_printable_latin1_binary");
        assert!(
            latin1.contains("124,60,60,48,44,49,44,50,53,53,62,62,124"),
            "{latin1}"
        );
        assert!(latin1.contains("60,60,34,92,34,92,92,34,62,62"), "{latin1}");
        assert!(latin1.ends_with("124,34,160,161,255,34]"), "{latin1}");
        // The left-adjusted form raises `badarg`; the case returns the *reason*.
        assert_eq!(golden("print/left_adjusted_badarg"), "ok badarg");
    }

    #[test]
    fn the_w_binary_probe_omits_the_depths_that_diverge() {
        let expr = expr("print/w_binary_depth_elision");
        // `~W` depth-elides only at depth 1 in the sibling runtime, so depths 2/3 are a
        // real divergence and must not be pinned; 0/1 and a depth past the element count do
        // agree.
        for depth in [0, 1, 4] {
            assert!(
                expr.contains(&format!("io_lib:format(\"~W\", [<<1,2,3>>, {depth}])")),
                "missing the agreeing ~W depth {depth}"
            );
        }
        for depth in [2, 3] {
            assert!(
                !expr.contains(&format!("io_lib:format(\"~W\", [<<1,2,3>>, {depth}])")),
                "~W depth {depth} diverges (not depth-elided) and must stay unpinned"
            );
        }
        // `~P` goes through `io_lib_pretty` and elides at every depth, so all are pinned.
        for depth in 0..=4 {
            assert!(
                expr.contains(&format!("io_lib:format(\"~P\", [<<1,2,3>>, {depth}])")),
                "missing the ~P depth {depth}"
            );
        }
    }
}
