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

//! Contract **A.1** — `erlang:error/2,3`'s stacktrace-side payload, and `erlang:raise/3`'s
//! **verbatim** supplied trace.
//!
//! `errors` pins the raise *class and reason*. This group pins the other half of an `error`
//! raise's observable image — the one the raised exception carries **about itself**:
//!
//! * **The caller frame's `A` slot is the exception's own argument list.** `error(Reason, Args)`
//!   stores `Args` (its *second* argument, verbatim) and the innermost cooked item renders that
//!   list where a plain raise renders an arity: `{M, F, [1,2], Location}`, never
//!   `{M, F, 2, Location}` (`errors_extra/error_2_fills_the_caller_frame_with_the_argument_list`).
//! * **`error/3` appends the `{error_info, Map}` extension.** Its option list is scanned (last
//!   `{error_info, Map}` wins; a non-map payload is dropped; a non-list, an improper list, a
//!   non-`{error_info,_}` element or a wrong arity invalidates the scan), and a kept map is
//!   consed **after** the `{file,_}`/`{line,_}` pairs, so the extended location is three cons
//!   cells (`errors_extra/error_3_option_matrix` reduces the whole scan matrix to one term,
//!   `errors_extra/error_3_appends_the_error_info_extension` pins the extension's position).
//! * **A supplied trace is echoed verbatim.** `erlang:raise/3` hands the engine the caller's
//!   trace *term*, and the cooked trace comes back **unchanged**: no decoding, no re-encoding,
//!   no frame prepended, no atom lookup and no fabricated frame for a module that does not
//!   exist. The term `[]` is word-identical to "nothing supplied", so the discriminator is the
//!   supplied marker's **presence**, not its value — `errors_extra/supplied_trace_is_echoed_verbatim`
//!   pins a supplied `[]` staying `[]` *and* an ordinary raise deriving a non-empty trace.
//!   `errors_extra/supplied_trace_unknown_module_frame_is_echoed_verbatim` pins the unknown
//!   module, and `errors_extra/proc_lib_re_raise_keeps_the_crash_trace` the `proc_lib`-style
//!   `catch Cls:Rsn:St -> erlang:raise(Cls,Rsn,St)` re-raise, whose echo is idempotent.
//!
//! # Why each expression spawns a closure
//!
//! These observables are the **cooked stacktrace**, and they are rendered by the engine's
//! raise path. The `-eval` route's tree walker cooks a trace from its own symbolic model: it
//! contributes no `erl_eval`/`init` frame of its own and does not render the `error/2,3`
//! payload, so a raise caught directly in `-eval` answers an empty trace on the Rust side.
//! The `spawn(fun() -> … end)` form, by contrast, is the runtime's documented in-process
//! **compile** route: the closure becomes real bytecode and runs on an engine machine, which
//! is exactly the tier that implements the payload and the verbatim echo. So every case here
//! spawns one closure, lets it catch the raise and send a projection back, and reads the
//! reply with a bounded `receive`.
//!
//! Only **stable** parts are projected: the `A` slot (`[1,2]`), the location's tag atoms
//! (`file`/`line`/`error_info`), the `{error_info, Map}` pair itself, and a supplied trace's
//! own terms. Never a `{file,_}` path, a line number, or a module/function frame: the oracle's
//! child is interpreted by `erl_eval` and the Rust runtime's is compiled, so those three
//! differ between the two sides while the pinned projections do not. The goldens are the
//! oracle's own stdout lines, measured with the corpus probe wrapper on `erl` 29.1.2 /
//! erts-17.1.1.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.1 `error/2,3` payload and `raise/3` supplied-trace cases.
pub(super) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "errors_extra/error_3_option_matrix",
            description: "the erlang:error/3 option scan keeps the last {error_info,Map} and drops every invalid options list, reduced to one term",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> Info = fun(Opts) -> Loc = element(4, hd(element(2, element(2, (catch error(boom,[1,2],Opts)))))), case [element(2,P) || P <- Loc, element(1,P) =:= error_info] of [] -> none; [M] -> {some, lists:sort(maps:keys(M))} end end, Self ! [Info([]), Info([{error_info, #{}}]), Info([{error_info, #{a=>1}}]), Info([{error_info, #{a=>1}},{error_info, #{b=>2}}]), Info([{error_info, bogus}]), Info([{bogus, 1}]), Info([bogus]), Info([{error_info, #{a=>1}} | tail]), Info(notalist), Info([{error_info, #{a=>1}, x}]), Info([{error_info, #{a=>1}}, bogus]), Info([[{error_info, #{a=>1}}]])] end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok [none,{some,[]},{some,[a]},{some,[b]},none,none,none,none,none,none,none,none]",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "errors_extra/error_2_fills_the_caller_frame_with_the_argument_list",
            description: "error/2 puts the exception's own argument list, not the arity, in the innermost stacktrace item's A slot",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> I = hd(element(2, element(2, (catch error(boom,[1,2]))))), Self ! {element(3,I), [element(1,X) || X <- element(4,I)]} end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1,2],[file,line]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors_extra/error_3_appends_the_error_info_extension",
            description: "error/3 appends one {error_info,Map} cons after the {file,_}/{line,_} pairs of the innermost item's location",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> I = hd(element(2, element(2, (catch error(boom,[1,2],[{error_info, #{a => 1}}]))))), Self ! {element(3,I), [element(1,X) || X <- element(4,I)], lists:last(element(4,I))} end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1,2],[file,line,error_info],{error_info,#{a => 1}}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors_extra/supplied_trace_is_echoed_verbatim",
            description: "erlang:raise/3 echoes a supplied trace verbatim and keeps a supplied [] (where an absent trace derives frames)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> A = element(2, element(2, (catch erlang:raise(error, boom, [])))), B = element(2, element(2, (catch erlang:raise(error, boom, [{m,f,[1,2],[]}])))), C = element(2, (catch error(boom))) =/= [], Self ! {A,B,C} end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[],[{m,f,[1,2],[]}],true}"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors_extra/supplied_trace_unknown_module_frame_is_echoed_verbatim",
            description: "a supplied frame naming a module that does not exist is echoed verbatim, with no fabricated replacement",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> Self ! element(2, element(2, (catch erlang:raise(error, boom, [{no_such_module_xyz, f, [1,2], []}])))) end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{no_such_module_xyz,f,[1,2],[]}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors_extra/proc_lib_re_raise_keeps_the_crash_trace",
            description: "a catch Cls:Rsn:St -> erlang:raise(Cls,Rsn,St) re-raise leaves the crash trace unchanged",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "(fun() -> Self = self(), spawn(fun() -> Self ! element(2, element(2, (catch (fun() -> try erlang:raise(error, boom, [{m,f,[1,2],[]}]) catch Cls:Rsn:St -> erlang:raise(Cls,Rsn,St) end end)()))) end), receive X -> X after 10000 -> timeout end end)()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{m,f,[1,2],[]}]"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact ids this group emits, in order — the size pin, and the spelling of every
    /// id, live here so a case cannot be added, retitled or dropped without this table.
    const IDS: [&str; 6] = [
        "errors_extra/error_3_option_matrix",
        "errors_extra/error_2_fills_the_caller_frame_with_the_argument_list",
        "errors_extra/error_3_appends_the_error_info_extension",
        "errors_extra/supplied_trace_is_echoed_verbatim",
        "errors_extra/supplied_trace_unknown_module_frame_is_echoed_verbatim",
        "errors_extra/proc_lib_re_raise_keeps_the_crash_trace",
    ];

    /// Every row is a caught form: the expression answers the observed term, so the probe
    /// prints `ok <term>` and the process exits `0` (an escaping raise would exit `1`).
    #[test]
    fn the_group_carries_exactly_these_ids_as_caught_value_cases() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len(), "the group size is pinned");
        for (case, id) in cases.iter().zip(IDS) {
            assert_eq!(case.id, id, "the id (and its order) is pinned");
            assert!(
                !case.description.is_empty(),
                "case {id}: the description is a one-line behaviour statement"
            );
            assert_eq!(
                case.contracts,
                &[Contract::LanguageSemantics],
                "case {id}: only the A.1 semantics contract"
            );
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {id}: must be an Eval case");
            };
            assert!(extra_argv.is_empty(), "case {id}: no extra argv");
            assert_eq!(probe, Probe::Value, "case {id}: a value rendering");
            assert_eq!(case.golden.exit_code, 0, "case {id}: the raise is caught");
            assert!(
                case.golden
                    .stdout
                    .is_some_and(|line| !line.is_empty() && line.starts_with("ok ")),
                "case {id}: the golden is the oracle's `ok <term>` line"
            );
            // The compiled-closure route is what makes the payload observable; a rewrite that
            // dropped the spawn (or the bounded reply) would silently stop pinning it.
            assert!(
                expr.contains("spawn(fun() ->"),
                "case {id}: the raise must be observed from a spawned compiled closure"
            );
            assert!(
                expr.contains("receive X -> X after 10000 -> timeout end"),
                "case {id}: the reply is read with a bounded receive"
            );
        }
    }

    /// The ids are unique and namespaced under the group's prefix.
    #[test]
    fn every_id_is_namespaced_and_unique() {
        let cases = group();
        let mut seen = std::collections::HashSet::new();
        for case in &cases {
            assert!(
                case.id.starts_with("errors_extra/"),
                "case {}: the id must be namespaced under errors_extra/",
                case.id
            );
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
        }
    }

    /// No case may pin a `{file,_}` **path** or a line number: the oracle's child is
    /// interpreted (`erl_eval.erl`) and the Rust runtime's is compiled (a synthetic module),
    /// so only the location's tag atoms are comparable.
    #[test]
    fn no_golden_pins_a_location_path_or_line_number() {
        for case in group() {
            let golden = case.golden.stdout.expect("every golden is asserted");
            for forbidden in ["erl_eval", ".erl", "{file,", "{line,"] {
                assert!(
                    !golden.contains(forbidden),
                    "case {}: the golden must not pin a location path or line number \
                     (`{forbidden}`): {golden}",
                    case.id
                );
            }
        }
    }

    /// The option matrix's expression must keep every row of the measured matrix — a rewrite
    /// that dropped a shape would leave the case green while pinning a smaller rule.
    #[test]
    fn the_option_matrix_keeps_every_measured_shape() {
        let cases = group();
        let case = cases
            .iter()
            .find(|case| case.id == "errors_extra/error_3_option_matrix")
            .expect("the matrix case is present");
        let CaseKind::Eval { expr, .. } = case.kind else {
            panic!("the matrix case must be an Eval case");
        };
        for shape in [
            "Info([])",
            "Info([{error_info, #{}}])",
            "Info([{error_info, #{a=>1}}])",
            "Info([{error_info, #{a=>1}},{error_info, #{b=>2}}])",
            "Info([{error_info, bogus}])",
            "Info([{bogus, 1}])",
            "Info([bogus])",
            "Info([{error_info, #{a=>1}} | tail])",
            "Info(notalist)",
            "Info([{error_info, #{a=>1}, x}])",
            "Info([{error_info, #{a=>1}}, bogus])",
            "Info([[{error_info, #{a=>1}}]])",
        ] {
            assert!(
                expr.contains(shape),
                "the option matrix lost the row `{shape}`"
            );
        }
    }
}
