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

//! Contract **A.1** — language semantics: arithmetic, term ordering, control flow,
//! pattern matching, funs, exceptions and comprehensions.
//!
//! These are the semantics a program *observes*: the value of an expression. The numeric
//! results are pinned through `term_to_binary/1` (the canonical encoding, contract A.2)
//! and the structural results through the `~w` rendering. Every golden here is the value
//! the reference system ERTS (`erl`) actually prints for the expression under the probe
//! wrapper ([`super::probe_expr`]).

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.1 semantics cases: arithmetic, ordering/equality, control flow, data structures.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "semantics/arith_precedence",
            description: "`*` binds tighter than `+` on integers",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "2+3*4",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,14>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/arith_left_associative",
            description: "`+`/`-` are left associative",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "10-3-2",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,5>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/integer_div",
            description: "`div` truncates toward zero",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "17 div 5",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/integer_rem",
            description: "`rem` has the sign of the dividend",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "17 rem 5",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/unary_minus",
            description: "unary minus negates an integer literal",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "-1",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,255,255,255,255>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/abs",
            description: "abs/1 of a negative integer is its magnitude",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "abs(-12)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,12>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/min_max",
            description: "min/2 and max/2 pick the smaller/larger of two integers",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{min(3,9), max(3,9)}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,97,3,97,9>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/bignum_multiplication",
            description: "integer arithmetic promotes to a bignum and stays exact",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "123456789012345678 * 98",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,8,0,220,35,189,158,245,114,231,167>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/arith_eq_across_types",
            description: "`==` compares numbers by value, so 1 == 1.0",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "1 == 1.0",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/exact_eq_across_types",
            description: "`=:=` distinguishes integer 1 from float 1.0",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "1 =:= 1.0",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,102,97,108,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/exact_ne_across_types",
            description: "`=/=` is the negation of `=:=`",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "1 =/= 1.0",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/order_number_before_atom",
            description: "term ordering puts numbers before atoms",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "5 < a",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/order_atom_before_tuple",
            description: "term ordering puts atoms before tuples",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "a < {1}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/order_tuple_before_list",
            description: "term ordering puts tuples before lists",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "{1} < [1]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/order_lists_lexicographic",
            description: "lists compare lexicographically element by element",
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
            id: "semantics/case_selects_a_clause",
            description: "a case expression selects the first matching clause",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "case 2 of 1 -> a; 2 -> b; _ -> c end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok b"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/if_selects_a_branch",
            description: "an if expression takes the first true branch",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "if 1 > 2 -> a; true -> b end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok b"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/try_catch_typed_error",
            description: "a try/catch clause matches class and reason (error:badarith)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try 1 div 0 catch error:badarith -> caught end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok caught"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/try_catch_throw_binds_reason",
            description: "a try/catch clause can bind the thrown reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try throw(x) catch throw:Y -> Y end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok x"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/fun_closure",
            description: "a fun closes over its environment and applies it",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin F = fun(X) -> X + 1 end, F(41) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/lists_map_fun",
            description: "lists:map/2 applies a fun to every element",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:map(fun(X) -> X*X end, [1,2,3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,4,9]"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/list_comprehension",
            description: "a list comprehension filters and maps generators",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "[X*2 || X <- [1,2,3]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [2,4,6]"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/binary_comprehension",
            description: "a binary comprehension maps each byte of a binary",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<2,3,4>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/pattern_match_tuple",
            description: "a tuple pattern binds its elements",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "case {1,2} of {A,B} -> A+B end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/list_head_and_length",
            description: "hd/1 and length/1 compose on a literal list",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "hd([9,8]) + length([1,2,3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 12"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/map_pattern_access",
            description: "maps:get/2 reads a key from a map literal",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "maps:get(a, #{a => 1, b => 2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/map_size",
            description: "map_size/1 counts the keys of a map literal",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "map_size(#{a => 1, b => 2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/tuple_element_atom",
            description: "element/2 returns any tuple element, including an atom",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(2, {a,b,c})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok b"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/binary_at",
            description: "binary:at/2 reads a byte of a binary",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary:at(<<10,20,30>>, 1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 20"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/bit_size_of_bitstring",
            description: "bit_size/1 counts the bits of a non-byte-aligned bitstring",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "bit_size(<<1:1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/is_binary",
            description: "is_binary/1 recognises a binary",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "is_binary(<<\"x\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/self_identity",
            description: "a process compares equal to itself with `=:=`",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "self() =:= self()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/paren_overrides_precedence",
            description: "parentheses override operator precedence: (2+3)*4 = 20",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(2+3)*4",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,20>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/negate_integer_literal",
            description: "unary minus applied to an integer literal is exact",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "-(3)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,255,255,255,253>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/andalso_short_circuit",
            description: "`andalso` is the short-circuit conjunction of booleans",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "true andalso false",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,102,97,108,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/tuple_order_second_element",
            description: "tuple ordering compares the second element when the first ties",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "{1,2} < {1,3}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/list_comprehension_filter",
            description: "a list comprehension generator with a filter selects the evens",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "[X || X <- [1,2,3,4], X rem 2 =:= 0]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [2,4]"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/fun_two_arg_closure",
            description: "a two-argument fun closes over its environment and applies it",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin F = fun(A,B) -> A*B end, F(6,7) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
    ]
}
