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

//! Contract **A.1** — the exception protocol: the three classes (`error`, `exit`,
//! `throw`) and the *reason term* each carries.
//!
//! The probe wrapper exists precisely for these: `erl -noshell -eval '<expr with an
//! uncaught error>'` prints **nothing** and exits `1`, so without the wrapper an
//! exception would be invisible to a stdout diff. With the wrapper a failure is one
//! observable line — `exception <class> <reason>` — and exit status `1`.
//!
//! Alongside the plain class cases, the group pins the **structured `error` reasons** a
//! raise carries (contract A.1). Each such case is `try (EXPR) catch error:Reason ->
//! Reason end`, so the raise is *caught inside the expression* and returned as an ordinary
//! value: the probe prints `ok <ETF of Reason>` with exit `0`, making the reason a
//! first-class, byte-exact observable (contract A.2's canonical encoding, observed through
//! A.1's protocol). The reason is asserted as its own term rather than the rendered
//! `exception error <reason>` line so the *structure* (tuple arity, which value the
//! failure carries) is pinned exactly.
//!
//! # Clause failure, `undef` and the remaining structured reasons
//!
//! Beyond the plain classes and the `{case_clause,_}`/`{badmatch,_}`/`if_clause` reasons the
//! group also pins the rest of the raise surface a program actually hits:
//!
//! * **clause failure** — a function head or guard that matches no clause raises the *bare
//!   atom* `function_clause` (`errors/function_clause_*`), including through a real compiled
//!   module ([`CLAUSE_FAILURE_SOURCE`], the `errors/module_*` cases). A module-run golden must
//!   be a plain integer line (the corpus invariant), so `classify/2` in that module maps the
//!   exact `(Class, Reason)` pair to a distinguishing integer — both runtimes execute the same
//!   `.beam`, so a divergent class/reason changes the printed integer and is a mismatch;
//! * **`undef`** — an unknown module, an unknown function of a known module, a known function
//!   called with the wrong arity and the `apply/3` path all raise the bare atom `undef`
//!   (`errors/undef_*`);
//! * **structured reasons** — `{badmap,_}`, `{badkey,_}`, `{bad_generator,_}`, `{try_clause,_}`,
//!   a second `{case_clause,_}` shape and the native-record `{badrecord,_}`.
//!
//! # Honest coverage (candidates deliberately *not* pinned here)
//!
//! Each of the following was verified against the live oracle and the Rust runtime and is a
//! real, observable **divergence** (a `mismatch`, i.e. a handoff to the runtime nodes, never a
//! capability gap), so it is documented rather than added:
//!
//! * `records:update/4` with a key naming no field — the oracle raises
//!   `error:{badfield, {{m,r}, zz}}` (a nested 2-tuple) while the Rust runtime raises
//!   `error:{badfield, {m,r}, zz}` (a flat 3-tuple): a different `term_to_binary/1` for the
//!   same reason. (`{badrecord,_}`, from a non-record argument or a module mismatch, *does*
//!   agree and is pinned below.)
//! * `5(1)` (calling a non-fun) — the oracle raises `error:{badfun, 5}` while the Rust runtime
//!   raises `error:badarg`.
//!
//! `records:get_definition/2` of a record no loaded module defines is *not* a divergence — the
//! runtime serves the lookup through its `RecordSource` bridge and the case `agree`s (pinned in
//! the sibling `corpus::records` group).

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The embedded module the clause-failure [`CaseKind::Module`] cases compile and run.
///
/// `head/0` calls a local function whose argument matches no head; `guard/0` calls a local
/// function whose only head is guarded. Both are the `func_info`/clause-failure path, whose
/// reason is the *bare atom* `function_clause`.
///
/// A module-run golden must be a plain integer line (the corpus invariant asserted in
/// `super::tests`), so the reason cannot be printed as a tuple: `classify/2` maps the exact
/// `(Class, Reason)` pair to a distinguishing integer (`1` for `error:function_clause`,
/// `10`/`20`/`30`/`40` for anything else). Both runtimes execute the *same* compiled module,
/// so a divergence in class or reason changes the printed integer — the case can never
/// silently pass on the wrong reason.
pub(crate) const CLAUSE_FAILURE_SOURCE: &str = "-module(errs_fc).\n\
     -export([head/0, guard/0]).\n\
     f(a) -> 1.\n\
     classify(error, function_clause) -> 1;\n\
     classify(error, _) -> 10;\n\
     classify(throw, _) -> 20;\n\
     classify(exit, _) -> 30;\n\
     classify(_, _) -> 40.\n\
     head() -> try f(b) catch Class:Reason -> classify(Class, Reason) end.\n\
     g(X) when is_integer(X), X > 0 -> X.\n\
     guard() -> try g(a) catch Class:Reason -> classify(Class, Reason) end.\n";

/// The A.1 exception cases: class + reason for `error`, `exit` and `throw`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "errors/throw_class",
            description: "throw/1 raises class throw with the thrown term as reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "throw(oops)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception throw oops"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/exit_normal",
            description: "exit/1 raises class exit with the given reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "exit(normal)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception exit normal"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/exit_tuple_reason",
            description: "exit/1 preserves a compound reason term",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "exit({shutdown,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception exit {shutdown,1}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/error_tuple_reason",
            description: "erlang:error/1 carries an arbitrary reason term",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error({bad,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {bad,1}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/hd_of_empty_list",
            description: "hd/1 of the empty list raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "hd([])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/element_out_of_range",
            description: "element/2 with an out-of-range index raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(5, {1,2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/list_to_integer_badarg",
            description: "list_to_integer/1 rejects a non-numeric list with error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "list_to_integer(\"x\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/tl_of_empty_list",
            description: "tl/1 of the empty list raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "tl([])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/element_zero_index",
            description: "element/2 with a zero index (one-based) raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(0, {1,2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/binary_to_integer_badarg",
            description: "binary_to_integer/1 rejects a non-digit binary with error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary_to_integer(<<120>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- Structured `error` reasons, caught in-expression and returned as a value ---
        // `try (EXPR) catch error:Reason -> Reason end` turns the raise into an ordinary
        // value, so the probe prints `ok <ETF of Reason>` (exit 0) and the reason term is
        // pinned byte-exactly (an untupled failure renders as the bare atom, e.g. `badarith`).
        Case {
            id: "errors/case_clause_reason",
            description: "a failed case match raises error:{case_clause,Value} carrying the scrutinee",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (case 1 of 2 -> x end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,11,99,97,115,101,95,99,108,97,117,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/case_clause_carries_compound_value",
            description: "the {case_clause,Value} reason preserves a compound (list) scrutinee",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (case [a,b] of [x] -> y end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,11,99,97,115,101,95,99,108,97,117,115,101,108,0,0,0,2,119,1,97,119,1,98,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/if_clause_reason",
            description: "an if with no true branch raises error:if_clause (an untupled reason)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin if false -> x end end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,9,105,102,95,99,108,97,117,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_reason",
            description: "a failed match raises error:{badmatch,Value} carrying the right-hand side",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin 5 = 6 end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,97,6>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_atom_value",
            description: "the {badmatch,Value} reason preserves an atom right-hand side",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin ok = error end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,119,5,101,114,114,111,114>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_tuple_value",
            description: "the {badmatch,Value} reason preserves a tuple whose size differs from the pattern",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin {A, B} = {1,2,3} end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,104,3,97,1,97,2,97,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_empty_list_value",
            description: "matching a cons pattern against [] raises error:{badmatch,[]}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin [H|T] = [] end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_binary_value",
            description: "matching a 1-byte binary pattern against a 2-byte binary raises {badmatch,Binary}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin <<X:8>> = <<1,2>> end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,109,0,0,0,2,1,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_map_literal_value",
            description: "a literal pattern against a map raises error:{badmatch,#{}} carrying the map",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin 2 = #{} end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,116,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_non_map_lhs_map_pattern",
            description: "an empty-map pattern against a non-map raises error:{badmatch,Value}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (#{} = 2) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badarith_reason",
            description: "arithmetic on a non-number raises error:badarith (an untupled reason)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (1 + a) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,8,98,97,100,97,114,105,116,104>>"),
                exit_code: 0,
            },
        },
        // --- Clause failure: the bare `function_clause` atom -------------------------------
        // A function head or guard that matches no clause raises the *untupled* atom
        // `function_clause` (opposite of `case_clause`/`badmatch`, which carry the value).
        Case {
            id: "errors/function_clause_improper_list",
            description: "lists:append/1 of an improper list raises error:function_clause (bare atom)",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (lists:append([a|b])) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,119,15,102,117,110,99,116,105,111,110,95,99,108,97,117,115,101>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/function_clause_non_list_argument",
            description: "lists:sum/1 of a non-list raises error:function_clause (bare atom)",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (lists:sum(a)) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,119,15,102,117,110,99,116,105,111,110,95,99,108,97,117,115,101>>",
                ),
                exit_code: 0,
            },
        },
        // The same path through a *real compiled module*: the local call's argument and the
        // guarded head each match nothing, so the module's `classify/2` must see
        // `error:function_clause` and print `1`. This is the path that used to loop forever.
        Case {
            id: "errors/module_local_call_function_clause",
            description: "a compiled module's local call with a non-matching head raises function_clause",
            contracts: &[Contract::BeamAsm, Contract::LanguageSemantics],
            kind: CaseKind::Module(ModuleRun {
                source: CLAUSE_FAILURE_SOURCE,
                module: "errs_fc",
                function: "head",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("1"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/module_guard_failure_function_clause",
            description: "a compiled module's guard that never holds raises function_clause",
            contracts: &[Contract::BeamAsm, Contract::LanguageSemantics],
            kind: CaseKind::Module(ModuleRun {
                source: CLAUSE_FAILURE_SOURCE,
                module: "errs_fc",
                function: "guard",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("1"),
                exit_code: 0,
            },
        },
        // --- `undef`: the bare atom for a missing module / function / arity ----------------
        Case {
            id: "errors/undef_unknown_module",
            description: "an unknown module raises error:undef (bare atom)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (nosuchmodule:nosuchfun(1)) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,117,110,100,101,102>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/undef_unknown_function",
            description: "an unknown function of a known module raises error:undef (bare atom)",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (lists:nosuchfun(1)) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,117,110,100,101,102>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/undef_wrong_arity",
            description: "a known function called with the wrong arity raises error:undef (bare atom)",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (lists:append(1,2,3)) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,117,110,100,101,102>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/undef_apply_unknown_function",
            description: "apply/3 of an unknown function raises error:undef (bare atom)",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (apply(lists, nosuch, [1])) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,117,110,100,101,102>>"),
                exit_code: 0,
            },
        },
        // --- The innermost `undef` stacktrace item (a callee-head frame) -------------------
        // A `catch` cannot bind the stacktrace directly (the runtime's `-eval` evaluator does
        // not support the three-argument `try C:R:Stacktrace ->` pattern), but the caught
        // `undef` reason is `{undef, Stacktrace}`, so the stacktrace — and its innermost item —
        // is reached through `element(2, element(2, (catch EXPR)))`. These cases pin that the
        // head item is `{Module, Function, ArgList, Location}`: the *third* element is the real
        // argument LIST (not an arity), which is what the engine now synthesizes for a
        // `call_ext` / `apply/3` raise.
        Case {
            id: "errors/undef_head_item_arg_list_from_apply",
            description: "an undef raised by apply/3 carries the innermost stacktrace item {Module,Function,ArgList,Location}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "element(3, hd(element(2, element(2, (catch apply(nonexistent_mod_xyz, foo, [1,2]))))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2]"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/undef_head_item_shape_from_apply",
            description: "the innermost undef stacktrace item has an atom Module, atom Function, list ArgList and list Location",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "begin St = element(2, element(2, (catch apply(nonexistent_mod_xyz, foo, [1,2])))), {is_atom(element(1,hd(St))), is_atom(element(2,hd(St))), is_list(element(3,hd(St))), element(3,hd(St))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,true,[1,2]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/undef_head_item_arg_list_from_call_ext",
            description: "an undef raised by a cross-module call carries the innermost stacktrace item with the real argument list",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "element(3, hd(element(2, element(2, (catch lists:no_such_fn_xyz(1))))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1]"),
                exit_code: 0,
            },
        },
        // --- The remaining structured `error` reasons --------------------------------------
        Case {
            id: "errors/badmap_reason",
            description: "maps:get/2 on a non-map raises error:{badmap,Value}",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (maps:get(a, not_a_map)) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,6,98,97,100,109,97,112,119,9,110,111,116,95,97,95,109,97,112>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmap_list_value",
            description: "the {badmap,Value} reason preserves a compound (list) non-map value",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (maps:get(a, [1,2])) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,6,98,97,100,109,97,112,107,0,2,1,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badkey_reason",
            description: "maps:get/2 of an absent key raises error:{badkey,Key}",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (maps:get(nokey, #{})) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,6,98,97,100,107,101,121,119,5,110,111,107,101,121>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badkey_compound_key",
            description: "the {badkey,Key} reason preserves a compound key",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (maps:get({a,b}, #{})) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,6,98,97,100,107,101,121,104,2,119,1,97,119,1,98>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/bad_generator_reason",
            description: "a list comprehension over a non-list raises error:{bad_generator,Value}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try ([Y || Y <- 5]) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,13,98,97,100,95,103,101,110,101,114,97,116,111,114,97,5>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/bad_generator_atom_value",
            description: "the {bad_generator,Value} reason preserves a non-integer non-list value",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try ([Y || Y <- not_a_list]) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,13,98,97,100,95,103,101,110,101,114,97,116,111,114,119,10,110,111,116,95,97,95,108,105,115,116>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/try_clause_reason",
            description: "a try … of with no matching clause raises error:{try_clause,Value}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin try 1 of 2 -> x catch error:Rsn -> Rsn end end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,10,116,114,121,95,99,108,97,117,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/case_clause_carries_map_value",
            description: "the {case_clause,Value} reason preserves a map scrutinee",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (case #{} of x -> y end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,11,99,97,115,101,95,99,108,97,117,115,101,116,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        // --- Native-record `{badrecord,_}` (the two shapes that agree on both runtimes) ----
        Case {
            id: "errors/badrecord_non_record_argument",
            description: "records:update/4 of a non-record raises error:{badrecord,Value}",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (records:update(5, m, r, #{a=>9})) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,9,98,97,100,114,101,99,111,114,100,97,5>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badrecord_module_mismatch",
            description: "records:update/4 with a module that is not the record's raises {badrecord,Name}",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try (records:update(records:create(m,r,[{a,1}],#{is_exported=>true}), n, r, #{a=>9})) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,9,98,97,100,114,101,99,111,114,100,119,1,114>>"),
                exit_code: 0,
            },
        },
        // --- Non-atom reasons for the explicit raise BIFs (uncaught ⇒ probe failure line) ---
        Case {
            id: "errors/error_list_reason",
            description: "erlang:error/1 carries a list reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error([a,b])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [a,b]"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/error_binary_reason",
            description: "erlang:error/1 carries a binary reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error(<<1,2>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error <<1,2>>"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/error_integer_reason",
            description: "erlang:error/1 carries an integer reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error(42)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error 42"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/error_two_arity_compound_reason",
            description: "erlang:error/2 raises the given non-atom reason with an argument list",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error({bad,2}, [x])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {bad,2}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/throw_tuple_reason",
            description: "throw/1 preserves a tuple reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "throw({a,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception throw {a,1}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/throw_list_reason",
            description: "throw/1 preserves a list reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "throw([1,2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception throw [1,2]"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/exit_nested_tuple_reason",
            description: "exit/1 preserves a nested tuple reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "exit({shutdown,{1,2}})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception exit {shutdown,{1,2}}"),
                exit_code: 1,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, Case, CaseKind, Contract, ModuleRun, Probe};

    /// The structured-reason cases this group adds. Each evaluates
    /// `try (EXPR) catch error:Reason -> Reason end`, so the raise is caught *in-expression*
    /// and the reason becomes an ordinary value (`ok <ETF of Reason>`, exit `0`).
    const STRUCTURED_REASON_CASES: &[&str] = &[
        "errors/case_clause_reason",
        "errors/case_clause_carries_compound_value",
        "errors/if_clause_reason",
        "errors/badmatch_reason",
        "errors/badmatch_atom_value",
        "errors/badmatch_tuple_value",
        "errors/badmatch_empty_list_value",
        "errors/badmatch_binary_value",
        "errors/badmatch_map_literal_value",
        "errors/badmatch_non_map_lhs_map_pattern",
        "errors/badarith_reason",
        // The remaining structured reasons (each a *distinct* reason term).
        "errors/badmap_reason",
        "errors/badmap_list_value",
        "errors/badkey_reason",
        "errors/badkey_compound_key",
        "errors/bad_generator_reason",
        "errors/bad_generator_atom_value",
        "errors/try_clause_reason",
        "errors/case_clause_carries_map_value",
        "errors/badrecord_non_record_argument",
        "errors/badrecord_module_mismatch",
    ];

    /// The clause-failure cases: a head/guard that matches nothing raises the bare atom
    /// `function_clause` (contract A.1) — the same reason for every case below.
    const FUNCTION_CLAUSE_CASES: &[&str] = &[
        "errors/function_clause_improper_list",
        "errors/function_clause_non_list_argument",
    ];

    /// The `undef` cases: an unknown module / function / arity (and the `apply/3` path) each
    /// raise the bare atom `undef`, so all four pin the *same* reason bytes.
    const UNDEF_CASES: &[&str] = &[
        "errors/undef_unknown_module",
        "errors/undef_unknown_function",
        "errors/undef_wrong_arity",
        "errors/undef_apply_unknown_function",
    ];

    /// The explicit raise BIFs whose reason is deliberately **not** an atom, observed
    /// *uncaught* through the probe's `exception <class> <reason>` failure line
    /// ([`Probe::Value`], exit `1`).
    const EXPLICIT_RAISE_CASES: &[&str] = &[
        "errors/error_list_reason",
        "errors/error_binary_reason",
        "errors/error_integer_reason",
        "errors/error_two_arity_compound_reason",
        "errors/throw_tuple_reason",
        "errors/throw_list_reason",
        "errors/exit_nested_tuple_reason",
    ];

    /// `ok <ETF>` of the bare `function_clause` atom (`term_to_binary(function_clause)`).
    const FUNCTION_CLAUSE_ETF: &str =
        "ok <<131,119,15,102,117,110,99,116,105,111,110,95,99,108,97,117,115,101>>";

    /// `ok <ETF>` of the bare `undef` atom (`term_to_binary(undef)`).
    const UNDEF_ETF: &str = "ok <<131,119,5,117,110,100,101,102>>";

    fn find<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the errors group"))
    }

    fn tokens(source: &str) -> impl Iterator<Item = &str> {
        source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
    }

    /// The inner expression must not reuse the probe wrapper's variables (`V`/`C`/`R`): a
    /// collision makes the oracle's `erl_eval` fail with `unsafe_var` (a false mismatch).
    fn assert_no_reserved_probe_variables(id: &str, expr: &str) {
        for var in ["V", "C", "R"] {
            assert!(
                !tokens(expr).any(|token| token == var),
                "case {id}: expression uses the reserved probe variable `{var}`"
            );
        }
    }

    #[test]
    fn structured_reason_cases_follow_the_caught_reason_form() {
        let cases = group();
        for id in STRUCTURED_REASON_CASES {
            let case = find(&cases, id);
            let CaseKind::Eval { expr, probe, .. } = case.kind else {
                panic!("case {id} must be an Eval case");
            };
            assert_eq!(probe, Probe::TermToBinary, "case {id}");
            assert!(
                expr.starts_with("try (") && expr.ends_with(") catch error:Reason -> Reason end"),
                "case {id}: unexpected caught-reason form {expr:?}"
            );
            assert_no_reserved_probe_variables(id, expr);
            assert_eq!(
                case.golden.exit_code, 0,
                "case {id}: a caught reason is a value"
            );
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert!(
                golden.starts_with("ok <<131,"),
                "case {id}: a caught reason is ETF-encoded, got {golden:?}"
            );
        }
    }

    #[test]
    fn structured_reason_cases_pin_distinct_reasons() {
        let cases = group();
        let mut reasons = std::collections::HashSet::new();
        for id in STRUCTURED_REASON_CASES {
            let golden = find(&cases, id).golden.stdout.expect("asserts stdout");
            assert!(
                reasons.insert(golden),
                "two structured-reason cases assert the same reason bytes: {golden:?}"
            );
        }
    }

    #[test]
    fn clause_failure_cases_pin_the_bare_reason_atom() {
        let cases = group();
        for (ids, reason, etf) in [
            (
                FUNCTION_CLAUSE_CASES,
                "function_clause",
                FUNCTION_CLAUSE_ETF,
            ),
            (UNDEF_CASES, "undef", UNDEF_ETF),
        ] {
            for id in ids {
                let case = find(&cases, id);
                let CaseKind::Eval { expr, probe, .. } = case.kind else {
                    panic!("case {id} must be an Eval case");
                };
                assert_eq!(probe, Probe::TermToBinary, "case {id}");
                assert!(
                    expr.starts_with("try (")
                        && expr.ends_with(") catch error:Reason -> Reason end"),
                    "case {id}: unexpected caught-reason form {expr:?}"
                );
                assert_no_reserved_probe_variables(id, expr);
                assert_eq!(
                    case.golden.exit_code, 0,
                    "case {id}: a caught reason is a value"
                );
                assert_eq!(
                    case.golden.stdout,
                    Some(etf),
                    "case {id}: {reason} must be the bare atom's ETF bytes"
                );
            }
        }
    }

    #[test]
    fn explicit_raise_cases_observe_a_non_atom_reason() {
        let cases = group();
        let mut reasons = std::collections::HashSet::new();
        for id in EXPLICIT_RAISE_CASES {
            let case = find(&cases, id);
            let CaseKind::Eval { expr, probe, .. } = case.kind else {
                panic!("case {id} must be an Eval case");
            };
            assert_eq!(probe, Probe::Value, "case {id}");
            assert!(
                expr.starts_with("erlang:error(")
                    || expr.starts_with("throw(")
                    || expr.starts_with("exit("),
                "case {id}: an explicit raise BIF drives the case, got {expr:?}"
            );
            assert_no_reserved_probe_variables(id, expr);
            assert_eq!(
                case.golden.exit_code, 1,
                "case {id}: an uncaught raise exits 1"
            );
            let golden = case.golden.stdout.expect("asserts stdout");
            assert!(
                golden.starts_with("exception error ")
                    || golden.starts_with("exception throw ")
                    || golden.starts_with("exception exit "),
                "case {id}: the probe's failure line is the observable, got {golden:?}"
            );
            assert!(
                reasons.insert(golden),
                "two explicit-raise cases assert the same reason rendering: {golden:?}"
            );
        }
    }

    /// The clause-failure *module* cases: the embedded module is compiled and run under both
    /// runtimes, and its `classify/2` maps `error:function_clause` to the integer `1`.
    #[test]
    fn clause_failure_module_cases_run_the_embedded_module() {
        let cases = group();
        for (id, function) in [
            ("errors/module_local_call_function_clause", "head"),
            ("errors/module_guard_failure_function_clause", "guard"),
        ] {
            let case = find(&cases, id);
            let CaseKind::Module(run) = case.kind else {
                panic!("case {id} must be a module-run case");
            };
            assert_eq!(
                run,
                ModuleRun {
                    source: super::CLAUSE_FAILURE_SOURCE,
                    module: "errs_fc",
                    function,
                    arity: 0,
                    args: &[],
                    nif: None,
                },
                "case {id}: the embedded module invocation"
            );
            assert!(
                run.source.contains("classify(error, function_clause) -> 1"),
                "case {id}: the module distinguishes the exact reason"
            );
            assert!(
                case.contracts.contains(&Contract::BeamAsm)
                    && case.contracts.contains(&Contract::LanguageSemantics),
                "case {id}: a module-run case evidences the .beam and semantics contracts"
            );
            assert_eq!(case.golden.stdout, Some("1"), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
        }
    }
}
