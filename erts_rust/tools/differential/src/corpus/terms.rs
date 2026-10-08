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

//! Contract **A.1** — language semantics: term ordering, equality and exceptions.
//!
//! Ordering and equality are observable through `term_to_binary/1` (the boolean result
//! encoded canonically), and the exception cases pin the *observable* form of a BEAM
//! failure — which is exactly why the probe wraps the expression in `try … catch`
//! (`erl -noshell -eval` prints nothing for an uncaught error).

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.1 cases: ordering/equality semantics and the exception protocol.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "terms/compare_int_lists",
            description: "list comparison is lexicographic on the elements",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "[1,2,3] < [1,2,4]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "terms/eq_vs_exact_eq",
            description: "== compares numerically while =:= compares term identity",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "{1 == 1.0, 1 =:= 1.0}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,4,116,114,117,101,119,5,102,97,108,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "terms/atom_order",
            description: "atoms compare by their code points (hello < world)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "hello < world",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/error_boom",
            description: "erlang:error/1 surfaces as class error with the given reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error(boom)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error boom"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/badarith",
            description: "integer division by zero raises error:badarith",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "1/0",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarith"),
                exit_code: 1,
            },
        },
    ]
}
