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

//! Contract **A.3** — the observable NIF surface.
//!
//! The NIF **C ABI** (`enif_*`) is not reachable through the black-box CLI: a NIF is
//! C code linked into the emulator, driven by the loader. What *is* observable is the
//! error surface a NIF exposes to Erlang code — `erlang:nif_error/1,2`, which a NIF
//! calls to raise a reason when it cannot fulfil a call. These cases pin that raise
//! exactly, over a spread of reason *shapes*: atoms, integers (incl. negative), floats,
//! binaries, lists (incl. nested and improper), maps, flat and nested tuples, and the
//! `/2` arity whose explicit `Location` list does not replace the reason; the structural
//! C-declaration half of A.3 (that a program referencing the `enif_*` declarations still
//! compiles) is asserted in `tests/abi.rs`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.3 cases: the `erlang:nif_error/1,2` raise surface.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "nif/nif_error_atom_reason",
            description: "erlang:nif_error/1 raises error with the given reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(foo)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error foo"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_tuple_reason",
            description: "erlang:nif_error/1 preserves a compound reason term",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({nif_not_loaded, m, f, 0})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {nif_not_loaded,m,f,0}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_default",
            description: "erlang:nif_error/1 with the conventional `undefined` reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(undefined)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error undefined"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_badarg_tuple_reason",
            description: "erlang:nif_error/1 carries the `{badarg, Args}` reason a NIF uses for bad input",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({badarg, [1,2,3]})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {badarg,[1,2,3]}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_nested_tuple_reason",
            description: "erlang:nif_error/1 preserves a nested compound reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({nif_not_loaded, m, f, {1,2}})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {nif_not_loaded,m,f,{1,2}}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_integer_reason",
            description: "erlang:nif_error/1 preserves a non-atom, non-tuple reason (integer)",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error 1"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_list_reason",
            description: "erlang:nif_error/1 preserves a list reason without string-rewriting it",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error([a,b,c])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [a,b,c]"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_two_uses_first_argument",
            description: "erlang:nif_error/2 raises with its first argument as the reason (the second is metadata)",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(foo, [1,2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error foo"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_deeply_nested_tuple_reason",
            description: "erlang:nif_error/1 preserves a deeply nested tuple reason unflattened",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({nif_not_loaded, {m, f, {1,2,3}}})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {nif_not_loaded,{m,f,{1,2,3}}}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_mixed_list_reason",
            description: "erlang:nif_error/1 preserves a list of mixed element shapes",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error([{nif_not_loaded,m,f,0}, badarg, 42])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [{nif_not_loaded,m,f,0},badarg,42]"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_improper_list_reason",
            description: "erlang:nif_error/1 preserves an improper list (non-list tail) reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error([a | b])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [a|b]"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_nested_list_reason",
            description: "erlang:nif_error/1 preserves a list of lists without flattening it",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error([[1,2],[3,4]])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [[1,2],[3,4]]"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_tuple_with_list_nesting_reason",
            description: "erlang:nif_error/1 preserves a tuple whose fields interleave lists and tuples",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({a, [b, {c, [d]}], e})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {a,[b,{c,[d]}],e}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_negative_integer_reason",
            description: "erlang:nif_error/1 preserves a negative integer reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(-7)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error -7"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_float_reason",
            description: "erlang:nif_error/1 preserves a float reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(1.5)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error 1.5"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_binary_reason",
            description: "erlang:nif_error/1 preserves a binary reason byte-for-byte",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(<<\"binary reason\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error <<98,105,110,97,114,121,32,114,101,97,115,111,110>>"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_single_key_map_reason",
            description: "erlang:nif_error/1 preserves a map reason (single-key, so order-stable)",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(#{nif => not_loaded})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error #{nif => not_loaded}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_two_with_explicit_location",
            description: "erlang:nif_error/2 raises with its first argument even with a module-location list",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(foo, [{m, f, 0}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error foo"),
                exit_code: 1,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 group: every case observes the `erlang:nif_error/1,2` raise
    //! (contract A.3) through the `value` probe, and each golden is the exact
    //! `exception error <reason>` line the oracle prints.

    use super::{group, CaseKind, Contract, Probe};

    #[test]
    fn every_case_is_a_nif_eval_case_raising_through_nif_error() {
        for case in group() {
            assert_eq!(case.contracts, &[Contract::NifAbi], "case {}", case.id);
            assert!(
                case.id.starts_with("nif/"),
                "case {} is outside the nif group",
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
                panic!("case {}: an A.3 case must be an eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                extra_argv.is_empty(),
                "case {} takes no extra argv",
                case.id
            );
            assert!(
                expr.starts_with("erlang:nif_error("),
                "case {}: A.3 is observed through erlang:nif_error/1,2, got {expr}",
                case.id
            );
            let golden = case.golden.stdout.expect("every case pins a golden");
            assert!(
                golden.starts_with("exception error "),
                "case {}: a nif_error raise is an error-class exception, got {golden:?}",
                case.id
            );
            assert_eq!(
                case.golden.exit_code, 1,
                "case {}: a raise exits 1",
                case.id
            );
        }
    }

    #[test]
    fn goldens_cover_distinct_reason_shapes() {
        let reasons: Vec<&str> = group()
            .iter()
            .filter_map(|case| case.golden.stdout)
            .filter_map(|line| line.strip_prefix("exception error "))
            .collect();
        for expected in [
            "foo",
            "{nif_not_loaded,m,f,0}",
            "{nif_not_loaded,{m,f,{1,2,3}}}",
            "[{nif_not_loaded,m,f,0},badarg,42]",
            "[a|b]",
            "[[1,2],[3,4]]",
            "{a,[b,{c,[d]}],e}",
            "-7",
            "1.5",
            "<<98,105,110,97,114,121,32,114,101,97,115,111,110>>",
            "#{nif => not_loaded}",
        ] {
            assert!(
                reasons.contains(&expected),
                "the A.3 surface no longer pins the reason shape {expected:?}"
            );
        }
    }
}
