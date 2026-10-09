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
//!
//! The `semantics/closure_*` cases pin the **closure calling convention**: a `fun`'s arguments
//! are passed *before* its captured free variables, so a capturing fun applied to a non-atom
//! argument — or a fun with several captures and several arguments of mixed types — only
//! observes the reference value while those two slot groups stay distinct. The numeric one
//! (`semantics/closure_capture_plus_numeric_arg`) pins the canonical `term_to_binary/1` image
//! (contract A.2) as well.

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
        // --- the closure calling convention (arguments BEFORE the free variables) ---
        //
        // A `fun` whose free variables are *also* bound at the call site is the shape that
        // regressed: the arguments and the captured environment are distinct slots of the
        // closure's calling convention, so any confusion between them changes the value.
        Case {
            id: "semantics/closure_capture_plus_numeric_arg",
            description: "a fun with a captured variable applied to a numeric argument \
                          (arguments before free variables)",
            contracts: &[Contract::LanguageSemantics, Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "begin Offset = 7, F = fun(X) -> X + Offset end, F(35) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,42>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_capture_plus_tuple_arg",
            description: "a fun capturing an atom, applied to a tuple argument",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Tag = ok, F = fun({A,B}) -> {Tag, A+B} end, F({20,22}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_curried_nested_applied",
            description: "a curried fun (a fun returning a fun) applied in two steps",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Adder = fun(X) -> fun(Y) -> X + Y end end, (Adder(40))(2) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_returned_from_fun_applied",
            description: "a fun returned from another fun, bound and then applied",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Mk = fun(N) -> fun(X) -> X * N end end, F = Mk(6), F(7) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_multiple_captures",
            description: "a fun carrying three captured variables, applied to two arguments",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin A = 2, B = 3, D = 4, F = fun(X,Q) -> A+B+D+X+Q end, F(10,23) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_capture_and_arg_types_differ",
            description: "captured atom + integer free variables, applied to a list argument",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Tag = tagged, Factor = 3, F = fun(L) -> {Tag, [E*Factor || E <- L]} end, \
                       F([1,2,3]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {tagged,[3,6,9]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_capture_arity_two_arg",
            description: "a fun capturing an integer, applied to a tuple and an integer argument",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Base = 100, F = fun({K,Val}, Mult) -> {K, Base + Val * Mult} end, \
                       F({score,7}, 6) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {score,142}"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_capture_list_arg_atom",
            description: "a fun capturing a list, applied to an atom argument",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin Prefix = [1,2], F = fun(TailAtom) -> {TailAtom, Prefix ++ [TailAtom]} end, \
                       F(tag) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {tag,[1,2,tag]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "semantics/closure_passed_to_lists_map",
            description: "a capturing fun passed to lists:map/2 keeps its environment",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin Factor = 5, lists:map(fun(X) -> X * Factor end, [1,2,3]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [5,10,15]"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.1 closure cases: every one is a self-terminating `Eval` case whose
    //! golden is the value the reference system ERTS printed for it, and **no** golden renders a
    //! per-VM-start handle (a pid/ref/port/fun image) that would drift on the next run.
    //!
    //! The table below pins the observable of each case (the closure **calling convention**:
    //! arguments first, captured free variables after) so a regression of the convention is
    //! caught by this group, not only by a live `differential run`.
    use super::{group, Case, CaseKind, Contract, Golden, Probe};

    /// `(id, kind label, golden stdout)` for every closure case added to the A.1 group.
    const CLOSURE_CASES: &[(&str, &str, &str)] = &[
        (
            "semantics/closure_capture_plus_numeric_arg",
            "term_to_binary",
            "ok <<131,97,42>>",
        ),
        (
            "semantics/closure_capture_plus_tuple_arg",
            "value",
            "ok {ok,42}",
        ),
        ("semantics/closure_curried_nested_applied", "value", "ok 42"),
        (
            "semantics/closure_returned_from_fun_applied",
            "value",
            "ok 42",
        ),
        ("semantics/closure_multiple_captures", "value", "ok 42"),
        (
            "semantics/closure_capture_and_arg_types_differ",
            "value",
            "ok {tagged,[3,6,9]}",
        ),
        (
            "semantics/closure_capture_arity_two_arg",
            "value",
            "ok {score,142}",
        ),
        (
            "semantics/closure_capture_list_arg_atom",
            "value",
            "ok {tag,[1,2,tag]}",
        ),
        (
            "semantics/closure_passed_to_lists_map",
            "value",
            "ok [5,10,15]",
        ),
    ];

    fn find(id: &str) -> Case {
        group()
            .into_iter()
            .find(|case| case.id == id)
            .unwrap_or_else(|| panic!("case {id} is not registered in the A.1 group"))
    }

    #[test]
    fn closure_cases_are_registered_eval_cases_with_the_expected_golden() {
        for (id, label, stdout) in CLOSURE_CASES {
            let case = find(id);
            assert!(
                case.contracts.contains(&Contract::LanguageSemantics),
                "case {id}: a closure case is A.1 language semantics"
            );
            assert!(!case.description.is_empty(), "case {id} has no description");
            assert_eq!(case.kind_label(), *label, "case {id}");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code: 0
                },
                "case {id}"
            );
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {id}: a closure case must be an eval case");
            };
            assert!(extra_argv.is_empty(), "case {id} takes no extra argv");
            assert!(
                !expr.is_empty() && !expr.contains('\n'),
                "case {id} has a suspicious expression"
            );
            assert!(
                expr.contains("fun("),
                "case {id}: a closure case must build a fun, got {expr}"
            );
            assert_eq!(
                probe,
                if *label == "term_to_binary" {
                    Probe::TermToBinary
                } else {
                    Probe::Value
                },
                "case {id}: the kind label and the probe agree"
            );
        }
    }

    #[test]
    fn closure_goldens_never_render_a_per_vm_start_handle() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case pins a golden");
            for volatile in ["<", "#Ref<", "#Port<", "#Pid<", "#Fun<"] {
                if volatile == "<" {
                    // Only the stable ETF image (`<<131,…>>`), never a raw pid/ref/port/fun.
                    assert!(
                        !golden.contains('#'),
                        "case {}: unexpected handle rendering in golden {golden:?}",
                        case.id
                    );
                    continue;
                }
                assert!(
                    !golden.contains(volatile),
                    "case {}: golden {golden:?} renders {volatile}",
                    case.id
                );
            }
        }
    }

    #[test]
    fn the_closure_group_pins_argument_and_capture_slots_separately() {
        // The shape that regressed: a free variable bound at the call site **and** a non-atom
        // argument. The capturing fun must be applied to something that is not an atom, so a
        // swapped argument/capture slot changes the result instead of coincidentally matching.
        let case = find("semantics/closure_capture_plus_numeric_arg");
        let expr = case.expr().expect("eval case");
        assert!(expr.contains("Offset = 7"), "case: {expr}");
        assert!(expr.contains("F(35)"), "case: {expr}");
        let case = find("semantics/closure_capture_plus_tuple_arg");
        let expr = case.expr().expect("eval case");
        assert!(expr.contains("Tag = ok"), "case: {expr}");
        assert!(expr.contains("F({20,22})"), "case: {expr}");
    }
}
