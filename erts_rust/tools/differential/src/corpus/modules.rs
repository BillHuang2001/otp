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

//! Contract **A.2** — `.beam` binary, and the **observable agreement** evidence.
//!
//! These are the corpus's agreement cases. Both kinds of case reach agreement: the
//! expression groups run the same `-eval` probe under both runtimes, and here a compiled
//! `.beam` runs within the Rust runtime's `run` action, so the two runtimes actually
//! produce — and are compared on — the same observable value.
//!
//! The module sources are embedded here and compiled at run time with the system `erlc`
//! (see [`crate::module_build`]). The harness invokes the oracle through
//! `-noshell -pa <dir> -eval …` and the Rust runtime through `run <dir>/<mod>.beam …`;
//! only stdout + exit status are compared.
//!
//! Every case in this group is **verified against both runtimes** (`differential run
//! <id>` → `AGREE`). The functions are deliberately restricted to what the Rust
//! runtime's `run` subset actually supports today — integer arithmetic, comparisons,
//! guards, recursion, `element/2`, and integer-returning stdlib calls — so the corpus
//! never claims agreement it does not have. Each case is tagged with every contract it
//! evidences: [`Contract::BeamAsm`] (a compiled `.beam` executes), plus
//! [`Contract::LanguageSemantics`] (arithmetic/control-flow semantics) or
//! [`Contract::OtpApps`] (a BIF/stdlib call) as applicable.
//!
//! The `modules/erl_error_format_exception_*` block additionally pins a **crash
//! regression**: `erl_error:format_exception/3` on a **heap-allocated** reason (the `{1,2}` /
//! `{a,b}` tuples) used to `SIGSEGV` the Rust runtime where the oracle answered a value, so
//! each of those cases now agrees. `modules/bif_base64_encode_empty_size` pins a second
//! regression on the `run` result path: a zero-length binary result (`base64:encode/1` of an
//! empty input, an off-heap refc binary) used to be refused (`beam_rt: badarg`, exit `1`).
//!
//! The `modules/undef_head_item_*` block pins the **callee-head `undef` stacktrace item** as the
//! engine synthesizes it from **real bytecode**: the three-argument `try C:R:St ->` pattern —
//! which the `-eval` evaluator cannot parse — binds the stacktrace directly, and its innermost
//! item must be the 4-tuple `{Module, Function, ArgList, Location}` (atom module, atom function,
//! the real argument list, a location list). `modules/fun_info_external_fun_created_in_compiled_code`
//! pins `erlang:fun_info/1` of a `fun ?MODULE:f/0` **built by compiled code** (an *external* fun:
//! exactly the `arity`/`env`/`module`/`name`/`type` keys and their values).
//!
//! `modules/timer_apply_after_message_is_delivered_in_compiled_code` pins the **fired** timer
//! from real bytecode: `timer:apply_after/4` must really send its message once the delay
//! elapses and the engine must wake the process waiting in the compiled `receive` (the `wait`/
//! `timeout` opcodes, under an `after` guard), so the function returns `42` — the `0` of the
//! timeout branch would be a `mismatch`.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The source compiled for the original `add`/`fib` cases.
///
/// It is deliberately tiny and uses only arithmetic and self-recursion, so it lowers to
/// the opcodes the Rust runtime already supports — the point is to compare a *real*
/// compiled `.beam`, not to stress a feature gap.
pub(crate) const DIFF_MOD_SOURCE: &str = "-module(diff_mod).\n\
     -export([add/2, fib/1]).\n\
     add(A,B) -> A+B.\n\
     fib(N) -> fib(N,0,1).\n\
     fib(0,A,_B) -> A;\n\
     fib(N,A,B) -> fib(N-1,B,A+B).\n";

/// The source compiled for the expanded arithmetic/semantics/BIF module-run cases.
///
/// Every exported function returns an **integer** (the shape a module-run golden pins)
/// and uses only operations the Rust runtime's `run` subset supports today. The
/// functions are grouped by the compatibility surface they exercise:
///
/// * arithmetic and comparison — contract A.1 (language semantics);
/// * guards, `case`, recursion — contract A.1;
/// * `element/2`/`tuple_size/1` and the `lists`/`maps`/`binary`/`base64` calls — contract A.7.
pub(crate) const DIFF_OPS_SOURCE: &str = "-module(diff_ops).\n\
     -export([add2/2, sub2/2, mul2/2, idiv2/2, irem2/2, neg1/1, iabs/1, precedence0/0,\n\
     \x20        mn2/2, mx2/2,\n\
     \x20        cmp_gt/2, cmp_lt/2, cmp_ge/2, cmp_eqe/2, cmp_eq/2, cmp_ne/2, cmp_chain/3,\n\
     \x20        guard_pos/1, guard_and/1, guard_or/1, guard_rem/1, guard_isint/1,\n\
     \x20        fib/1, fact/2, sumto/1, gcd/2, ack/2, pow/2, collatz/1,\n\
     \x20        elem2_0/0, tsize0/0,\n\
     \x20        len0/0, sumlist0/0, nth2/1, revlen0/0, maxlist0/0, bsize0/0, decode0/0,\n\
     \x20        msize0/0, mapget0/0, seqsum/1,\n\
     \x20        foldl_sum0/0, map_len0/0, append_len0/0, rev_head0/0, tuple_elem3_0/0,\n\
     \x20        bin_at0/0, bit_size0/0, maps_get_k0/0, member10/0, seq_len0/0,\n\
     \x20        lists_last0/0, int_to_list_len0/0, sum_range/2, b64_empty_size/1]).\n\
     add2(A,B) -> A+B.\n\
     sub2(A,B) -> A-B.\n\
     mul2(A,B) -> A*B.\n\
     idiv2(A,B) -> A div B.\n\
     irem2(A,B) -> A rem B.\n\
     neg1(N) -> -N.\n\
     iabs(N) -> abs(N).\n\
     precedence0() -> 2 + 3 * 4.\n\
     mn2(A,B) -> min(A,B).\n\
     mx2(A,B) -> max(A,B).\n\
     cmp_gt(A,B) -> case A > B of true -> 1; false -> 0 end.\n\
     cmp_lt(A,B) -> case A < B of true -> 1; false -> 0 end.\n\
     cmp_ge(A,B) -> case A >= B of true -> 1; false -> 0 end.\n\
     cmp_eqe(A,B) -> case A =:= B of true -> 1; false -> 0 end.\n\
     cmp_eq(A,B) -> case A == B of true -> 1; false -> 0 end.\n\
     cmp_ne(A,B) -> case A /= B of true -> 1; false -> 0 end.\n\
     cmp_chain(A,B,C) -> case (A < B) andalso (B < C) of true -> 1; false -> 0 end.\n\
     guard_pos(N) when N > 0 -> 1;\n\
     guard_pos(_) -> 0.\n\
     guard_and(N) when N > 0, N < 100 -> 1;\n\
     guard_and(_) -> 0.\n\
     guard_or(N) when N < 0; N > 100 -> 1;\n\
     guard_or(_) -> 0.\n\
     guard_rem(N) when N rem 2 =:= 0 -> 0;\n\
     guard_rem(_) -> 1.\n\
     guard_isint(N) when is_integer(N) -> 1;\n\
     guard_isint(_) -> 0.\n\
     fib(N) -> fib(N, 0, 1).\n\
     fib(0, A, _) -> A;\n\
     fib(N, A, B) -> fib(N - 1, B, A + B).\n\
     fact(N, A) when N =< 1 -> A;\n\
     fact(N, A) -> fact(N - 1, N * A).\n\
     sumto(0) -> 0;\n\
     sumto(N) -> N + sumto(N - 1).\n\
     gcd(A, 0) -> A;\n\
     gcd(A, B) -> gcd(B, A rem B).\n\
     ack(0, N) -> N + 1;\n\
     ack(M, 0) -> ack(M - 1, 1);\n\
     ack(M, N) -> ack(M - 1, ack(M, N - 1)).\n\
     pow(_, 0) -> 1;\n\
     pow(B, E) -> B * pow(B, E - 1).\n\
     collatz(N) -> collatz(N, 0).\n\
     collatz(1, C) -> C;\n\
     collatz(N, C) when N rem 2 =:= 0 -> collatz(N div 2, C + 1);\n\
     collatz(N, C) -> collatz(3 * N + 1, C + 1).\n\
     elem2_0() -> element(2, {100,200,300}).\n\
     tsize0() -> tuple_size({1,2,3,4}).\n\
     len0() -> length([1,2,3,4,5]).\n\
     sumlist0() -> lists:sum([1,2,3,4]).\n\
     nth2(I) -> lists:nth(I, [10,20,30,40]).\n\
     revlen0() -> length(lists:reverse([1,2,3])).\n\
     maxlist0() -> lists:max([3,9,4]).\n\
     bsize0() -> byte_size(<<1,2,3,4>>).\n\
     decode0() -> binary:decode_unsigned(<<1,2>>).\n\
     msize0() -> map_size(#{a => 1, b => 2}).\n\
     mapget0() -> maps:get(b, #{a => 1, b => 2}).\n\
     seqsum(N) -> lists:sum(lists:seq(1, N)).\n\
     foldl_sum0() -> lists:foldl(fun(X, A) -> X + A end, 0, [1,2,3,4]).\n\
     map_len0() -> length(lists:map(fun(X) -> X * 2 end, [1,2,3])).\n\
     append_len0() -> length([1,2] ++ [3,4]).\n\
     rev_head0() -> hd(lists:reverse([1,2,3])).\n\
     tuple_elem3_0() -> element(3, {a,b,42}).\n\
     bin_at0() -> binary:at(<<10,20,30>>, 1).\n\
     bit_size0() -> bit_size(<<1,2,3>>).\n\
     maps_get_k0() -> maps:get(k, #{k => 9}).\n\
     member10() -> case lists:member(2, [1,2,3]) of true -> 1; false -> 0 end.\n\
     seq_len0() -> length(lists:seq(1, 10)).\n\
     lists_last0() -> lists:last([1,2,3]).\n\
     int_to_list_len0() -> length(integer_to_list(12345)).\n\
     sum_range(A, B) when A > B -> 0;\n\
     sum_range(A, B) -> A + sum_range(A + 1, B).\n\
     b64_empty_size(N) -> byte_size(base64:encode(binary:copy(<<N>>, 0))).\n";

/// The source compiled for the `erl_error:format_exception/3` regression cases.
///
/// Each exported function hands a reason to `erl_error:format_exception/3` and reduces the
/// (deep) formatted charlist to its **flattened length**, an integer — the shape a module-run
/// golden pins. The `{1,2}` / `{a,b}` reasons are **heap-allocated** tuples, which is exactly
/// the shape that used to `SIGSEGV` the Rust runtime (the heap-allocated reason was walked
/// through a stale pointer) while the oracle returned a value; `badarg` is an immediate atom.
pub(crate) const DIFF_ERL_ERROR_SOURCE: &str = "-module(diff_erl_error).\n\
     -export([fmt_throw_tuple/0, fmt_throw_atom_tuple/0, fmt_error_badarg/0]).\n\
     fmt_throw_tuple() -> length(lists:flatten(erl_error:format_exception(throw, {1,2}, []))).\n\
     fmt_throw_atom_tuple() -> length(lists:flatten(erl_error:format_exception(throw, {a,b}, []))).\n\
     fmt_error_badarg() -> length(lists:flatten(erl_error:format_exception(error, badarg, []))).\n";

/// The source compiled for the `undef`-stacktrace / `fun_info` module-run cases.
///
/// These are shapes the runtime's `-eval` evaluator cannot express — it rejects the
/// three-argument `try C:R:St ->` pattern (the innermost `undef` stacktrace item is therefore
/// only reachable from **real bytecode**) and it cannot build a *local* fun literal — so the
/// cases drive them through a compiled module. Every exported function returns an **integer**
/// (the single shape a module-run golden pins) and reduces its observation to a bit mask over
/// the properties being checked, so the golden is format- and locale-stable:
///
/// * `undef_head_shape/0` — the callee-head item's property mask plus `100 * length(Args)`;
/// * `undef_head_arg_list/0` — the head item's `Module`/`Function`/`ArgList` identity;
/// * `external_fun_info/0` — `erlang:fun_info/1` of a `fun ?MODULE:f/0` created by compiled
///   code (an *external* fun), reduced to its five-key list plus each value.
pub(crate) const DIFF_TRACE_SOURCE: &str = "-module(diff_trace_info).\n\
     -export([undef_head_shape/0, undef_head_arg_list/0, external_fun_info/0]).\n\
     undef_head_shape() ->\n\
     \x20   try lists:nonexistent_fn_xyz(1,2) of\n\
     \x20       _ -> 0\n\
     \x20   catch\n\
     \x20       _C:_R:St ->\n\
     \x20           {M,F,Args,Loc} = hd(St),\n\
     \x20           bool2int(is_atom(M)) + 2 * bool2int(is_atom(F))\n\
     \x20               + 4 * bool2int(is_list(Args)) + 8 * bool2int(is_list(Loc))\n\
     \x20               + 100 * length(Args)\n\
     \x20   end.\n\
     undef_head_arg_list() ->\n\
     \x20   try lists:nonexistent_fn_xyz(1,2) of\n\
     \x20       _ -> 0\n\
     \x20   catch\n\
     \x20       _C:_R:St ->\n\
     \x20           {M,F,Args,_Loc} = hd(St),\n\
     \x20           bool2int(M =:= lists) + 2 * bool2int(F =:= nonexistent_fn_xyz)\n\
     \x20               + 4 * bool2int(Args =:= [1,2])\n\
     \x20   end.\n\
     external_fun_info() ->\n\
     \x20   I = erlang:fun_info(fun ?MODULE:undef_head_shape/0),\n\
     \x20   Keys = lists:sort([K || {K,_} <- I]),\n\
     \x20   bool2int(Keys =:= [arity, env, module, name, type])\n\
     \x20       + 2 * bool2int(proplists:get_value(arity, I) =:= 0)\n\
     \x20       + 4 * bool2int(proplists:get_value(module, I) =:= diff_trace_info)\n\
     \x20       + 8 * bool2int(proplists:get_value(name, I) =:= undef_head_shape)\n\
     \x20       + 16 * bool2int(proplists:get_value(type, I) =:= external).\n\
     bool2int(true) -> 1;\n\
     bool2int(false) -> 0.\n";

/// The source of the fired-timer module.
///
/// A timer is only observable once it **fires**, and the message it sends must arrive at the
/// receiving process — a shape the `-eval` route pins as well
/// (`bifs/timer_apply_after_message_is_delivered`), but from *real bytecode* here: the
/// `receive` is compiled to the real `wait`/`timeout` opcodes rather than interpreted, so the
/// module-run route pins that the scheduler really wakes the process when the timer expires.
/// The exported function reduces the observation to an **integer** (the shape a module-run
/// golden pins): `42` when the scheduled `erlang:send(self(), fired)` message arrived, `0` when
/// nothing arrived within the `after` guard. The timer's reference is discarded (`_ =`) — a
/// per-VM value is never rendered.
pub(crate) const DIFF_TIMER_SOURCE: &str = "-module(diff_timer).\n\
     -export([wait_fired/0]).\n\
     wait_fired() ->\n\
     \x20   _ = timer:apply_after(50, erlang, send, [self(), fired]),\n\
     \x20   receive fired -> 42 after 2000 -> 0 end.\n";

/// Build one module-run case.
fn case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    function: &'static str,
    arity: u32,
    args: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Module(ModuleRun {
            source: DIFF_OPS_SOURCE,
            module: "diff_ops",
            function,
            arity,
            args,
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// Build one `erl_error:format_exception/3` module-run case (arity-0, integer result).
fn err_case(
    id: &'static str,
    description: &'static str,
    function: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::BeamAsm, Contract::OtpApps],
        kind: CaseKind::Module(ModuleRun {
            source: DIFF_ERL_ERROR_SOURCE,
            module: "diff_erl_error",
            function,
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// Build one `undef`-stacktrace / `fun_info` module-run case (arity-0, integer result).
fn trace_case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    function: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Module(ModuleRun {
            source: DIFF_TRACE_SOURCE,
            module: "diff_trace_info",
            function,
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}
/// The A.2 module-run cases. Every one of these is verified `AGREE`.
pub(crate) fn group() -> Vec<Case> {
    use Contract::{BeamAsm, LanguageSemantics, OtpApps};
    vec![
        // --- The original A.2 agreement pair -------------------------------------------
        Case {
            id: "modules/add",
            description: "a freshly erlc-compiled module's add/2 returns the same value under both runtimes",
            contracts: &[BeamAsm, LanguageSemantics],
            kind: CaseKind::Module(ModuleRun {
                source: DIFF_MOD_SOURCE,
                module: "diff_mod",
                function: "add",
                arity: 2,
                args: &[2, 3],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("5"),
                exit_code: 0,
            },
        },
        Case {
            id: "modules/fib",
            description: "a freshly erlc-compiled module's fib/1 returns the same value under both runtimes",
            contracts: &[BeamAsm, LanguageSemantics],
            kind: CaseKind::Module(ModuleRun {
                source: DIFF_MOD_SOURCE,
                module: "diff_mod",
                function: "fib",
                arity: 1,
                args: &[10],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("55"),
                exit_code: 0,
            },
        },
        // --- A.1: integer arithmetic --------------------------------------------------
        case(
            "modules/arith_add",
            "integer +/2 agrees on non-negative operands",
            &[BeamAsm, LanguageSemantics],
            "add2",
            2,
            &[40, 2],
            "42",
        ),
        case(
            "modules/arith_add_negative",
            "integer +/2 agrees when one operand is negative",
            &[BeamAsm, LanguageSemantics],
            "add2",
            2,
            &[-5, 7],
            "2",
        ),
        case(
            "modules/arith_sub",
            "integer -/2 agrees on non-negative operands",
            &[BeamAsm, LanguageSemantics],
            "sub2",
            2,
            &[10, 3],
            "7",
        ),
        case(
            "modules/arith_mul",
            "integer */2 agrees on non-negative operands",
            &[BeamAsm, LanguageSemantics],
            "mul2",
            2,
            &[6, 7],
            "42",
        ),
        case(
            "modules/arith_mul_double_negative",
            "integer */2 agrees when both operands are negative",
            &[BeamAsm, LanguageSemantics],
            "mul2",
            2,
            &[-6, -7],
            "42",
        ),
        case(
            "modules/arith_div",
            "integer div/2 truncates toward zero",
            &[BeamAsm, LanguageSemantics],
            "idiv2",
            2,
            &[100, 7],
            "14",
        ),
        case(
            "modules/arith_div_negative_dividend",
            "integer div/2 with a negative dividend truncates toward zero",
            &[BeamAsm, LanguageSemantics],
            "idiv2",
            2,
            &[-100, 7],
            "-14",
        ),
        case(
            "modules/arith_rem",
            "integer rem/2 has the sign of the dividend",
            &[BeamAsm, LanguageSemantics],
            "irem2",
            2,
            &[100, 7],
            "2",
        ),
        case(
            "modules/arith_rem_negative_dividend",
            "integer rem/2 with a negative dividend keeps the dividend's sign",
            &[BeamAsm, LanguageSemantics],
            "irem2",
            2,
            &[-100, 7],
            "-2",
        ),
        case(
            "modules/arith_negate",
            "unary minus negates an integer",
            &[BeamAsm, LanguageSemantics],
            "neg1",
            1,
            &[42],
            "-42",
        ),
        case(
            "modules/arith_abs",
            "abs/1 of a negative integer is its magnitude",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "iabs",
            1,
            &[-42],
            "42",
        ),
        case(
            "modules/arith_precedence",
            "`*` binds tighter than `+` in compiled code",
            &[BeamAsm, LanguageSemantics],
            "precedence0",
            0,
            &[],
            "14",
        ),
        case(
            "modules/arith_min",
            "min/2 picks the smaller integer",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "mn2",
            2,
            &[3, 9],
            "3",
        ),
        case(
            "modules/arith_max",
            "max/2 picks the larger integer",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "mx2",
            2,
            &[3, 9],
            "9",
        ),
        // --- A.1: comparison expressed as an integer result ---------------------------
        case(
            "modules/cmp_gt_true",
            "`>` is true for a larger left operand (encoded as 1)",
            &[BeamAsm, LanguageSemantics],
            "cmp_gt",
            2,
            &[5, 3],
            "1",
        ),
        case(
            "modules/cmp_gt_false",
            "`>` is false for a smaller left operand (encoded as 0)",
            &[BeamAsm, LanguageSemantics],
            "cmp_gt",
            2,
            &[3, 5],
            "0",
        ),
        case(
            "modules/cmp_lt",
            "`<` is true for a smaller left operand",
            &[BeamAsm, LanguageSemantics],
            "cmp_lt",
            2,
            &[3, 5],
            "1",
        ),
        case(
            "modules/cmp_ge_equal",
            "`>=` is true when the operands are equal",
            &[BeamAsm, LanguageSemantics],
            "cmp_ge",
            2,
            &[5, 5],
            "1",
        ),
        case(
            "modules/cmp_exact_eq",
            "`=:=` is true for identical integers",
            &[BeamAsm, LanguageSemantics],
            "cmp_eqe",
            2,
            &[5, 5],
            "1",
        ),
        case(
            "modules/cmp_eq_false",
            "`==` is false for distinct integers",
            &[BeamAsm, LanguageSemantics],
            "cmp_eq",
            2,
            &[5, 6],
            "0",
        ),
        case(
            "modules/cmp_ne",
            "`/=` is true for distinct integers",
            &[BeamAsm, LanguageSemantics],
            "cmp_ne",
            2,
            &[5, 6],
            "1",
        ),
        case(
            "modules/cmp_conjunction",
            "a conjunction of comparisons (andalso) is true for an ordered triple",
            &[BeamAsm, LanguageSemantics],
            "cmp_chain",
            3,
            &[1, 2, 3],
            "1",
        ),
        // --- A.1: guards --------------------------------------------------------------
        case(
            "modules/guard_positive",
            "a `N > 0` guard selects the first clause",
            &[BeamAsm, LanguageSemantics],
            "guard_pos",
            1,
            &[3],
            "1",
        ),
        case(
            "modules/guard_fallthrough",
            "a failing guard falls through to the next clause",
            &[BeamAsm, LanguageSemantics],
            "guard_pos",
            1,
            &[-3],
            "0",
        ),
        case(
            "modules/guard_conjunction",
            "a comma-separated guard conjunction (N > 0, N < 100) holds",
            &[BeamAsm, LanguageSemantics],
            "guard_and",
            1,
            &[50],
            "1",
        ),
        case(
            "modules/guard_conjunction_miss",
            "a comma-separated guard conjunction fails on the second test",
            &[BeamAsm, LanguageSemantics],
            "guard_and",
            1,
            &[500],
            "0",
        ),
        case(
            "modules/guard_disjunction",
            "a semicolon-separated guard disjunction (N < 0; N > 100) holds",
            &[BeamAsm, LanguageSemantics],
            "guard_or",
            1,
            &[500],
            "1",
        ),
        case(
            "modules/guard_rem",
            "a `rem` guard classifies an odd integer",
            &[BeamAsm, LanguageSemantics],
            "guard_rem",
            1,
            &[9],
            "1",
        ),
        case(
            "modules/guard_is_integer",
            "an is_integer/1 guard succeeds for an integer argument",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "guard_isint",
            1,
            &[7],
            "1",
        ),
        // --- A.1: recursion -----------------------------------------------------------
        case(
            "modules/fib_tail_recursive",
            "a tail-recursive fib/1 agrees on fib(20)",
            &[BeamAsm, LanguageSemantics],
            "fib",
            1,
            &[20],
            "6765",
        ),
        case(
            "modules/factorial_accumulator",
            "an accumulator factorial agrees on 10!",
            &[BeamAsm, LanguageSemantics],
            "fact",
            2,
            &[10, 1],
            "3628800",
        ),
        case(
            "modules/sum_recursive",
            "a non-tail-recursive sum agrees on 1+...+100",
            &[BeamAsm, LanguageSemantics],
            "sumto",
            1,
            &[100],
            "5050",
        ),
        case(
            "modules/gcd_euclid",
            "Euclid's algorithm agrees on gcd(48,36)",
            &[BeamAsm, LanguageSemantics],
            "gcd",
            2,
            &[48, 36],
            "12",
        ),
        case(
            "modules/ackermann",
            "the Ackermann function agrees on ack(2,3)",
            &[BeamAsm, LanguageSemantics],
            "ack",
            2,
            &[2, 3],
            "9",
        ),
        case(
            "modules/power_recursive",
            "a recursive integer power agrees on 3^5",
            &[BeamAsm, LanguageSemantics],
            "pow",
            2,
            &[3, 5],
            "243",
        ),
        case(
            "modules/collatz_steps",
            "the Collatz step count agrees for 27",
            &[BeamAsm, LanguageSemantics],
            "collatz",
            1,
            &[27],
            "111",
        ),
        // --- A.1/A.7: term operations -------------------------------------------------
        case(
            "modules/element_literal",
            "element/2 reads a literal tuple element",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "elem2_0",
            0,
            &[],
            "200",
        ),
        case(
            "modules/tuple_size_literal",
            "tuple_size/1 reports the arity of a literal tuple",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "tsize0",
            0,
            &[],
            "4",
        ),
        // --- A.7: BIF / stdlib calls returning an integer -----------------------------
        case(
            "modules/bif_length",
            "length/1 counts a literal list",
            &[BeamAsm, OtpApps],
            "len0",
            0,
            &[],
            "5",
        ),
        case(
            "modules/bif_lists_sum",
            "lists:sum/1 adds a literal list",
            &[BeamAsm, OtpApps],
            "sumlist0",
            0,
            &[],
            "10",
        ),
        case(
            "modules/bif_lists_nth",
            "lists:nth/2 indexes a literal list",
            &[BeamAsm, OtpApps],
            "nth2",
            1,
            &[3],
            "30",
        ),
        case(
            "modules/bif_lists_reverse_length",
            "length/1 (lists:reverse/1) agrees on a reversed list's length",
            &[BeamAsm, OtpApps],
            "revlen0",
            0,
            &[],
            "3",
        ),
        case(
            "modules/bif_lists_max",
            "lists:max/1 finds the largest element",
            &[BeamAsm, OtpApps],
            "maxlist0",
            0,
            &[],
            "9",
        ),
        case(
            "modules/bif_byte_size",
            "byte_size/1 reports a literal binary's size",
            &[BeamAsm, OtpApps],
            "bsize0",
            0,
            &[],
            "4",
        ),
        case(
            "modules/bif_binary_decode_unsigned",
            "binary:decode_unsigned/1 decodes a big-endian integer",
            &[BeamAsm, OtpApps],
            "decode0",
            0,
            &[],
            "258",
        ),
        case(
            "modules/bif_map_size",
            "map_size/1 counts a literal map's keys",
            &[BeamAsm, OtpApps],
            "msize0",
            0,
            &[],
            "2",
        ),
        case(
            "modules/bif_maps_get",
            "maps:get/2 reads a literal map's value",
            &[BeamAsm, OtpApps],
            "mapget0",
            0,
            &[],
            "2",
        ),
        case(
            "modules/bif_lists_seq_sum",
            "lists:sum/1 (lists:seq/2) agrees on 1+...+5",
            &[BeamAsm, OtpApps],
            "seqsum",
            1,
            &[5],
            "15",
        ),
        // --- A.1/A.7: higher-order stdlib calls and further term operations ------------
        case(
            "modules/bif_lists_foldl_fun",
            "lists:foldl/3 with a reducing fun agrees on 1+2+3+4",
            &[BeamAsm, OtpApps],
            "foldl_sum0",
            0,
            &[],
            "10",
        ),
        case(
            "modules/bif_lists_map_fun_length",
            "length/1 of lists:map/2 with a fun agrees",
            &[BeamAsm, OtpApps],
            "map_len0",
            0,
            &[],
            "3",
        ),
        case(
            "modules/list_append_length",
            "list ++/2 concatenation agrees on the combined length",
            &[BeamAsm, LanguageSemantics],
            "append_len0",
            0,
            &[],
            "4",
        ),
        case(
            "modules/bif_lists_reverse_head",
            "hd/1 of lists:reverse/1 agrees",
            &[BeamAsm, OtpApps],
            "rev_head0",
            0,
            &[],
            "3",
        ),
        case(
            "modules/tuple_element_third",
            "element/2 reads the third element of a literal tuple",
            &[BeamAsm, OtpApps],
            "tuple_elem3_0",
            0,
            &[],
            "42",
        ),
        case(
            "modules/bif_binary_at_middle",
            "binary:at/2 reads a middle byte of a literal binary",
            &[BeamAsm, OtpApps],
            "bin_at0",
            0,
            &[],
            "20",
        ),
        case(
            "modules/bif_bit_size",
            "bit_size/1 of a 3-byte binary agrees on 24",
            &[BeamAsm, OtpApps],
            "bit_size0",
            0,
            &[],
            "24",
        ),
        case(
            "modules/bif_maps_get_key",
            "maps:get/2 reads a literal map's key",
            &[BeamAsm, OtpApps],
            "maps_get_k0",
            0,
            &[],
            "9",
        ),
        case(
            "modules/bif_lists_member_case",
            "lists:member/2 drives a compiled case expression",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "member10",
            0,
            &[],
            "1",
        ),
        case(
            "modules/bif_lists_seq_length",
            "length/1 of lists:seq/2 agrees on 1..10",
            &[BeamAsm, OtpApps],
            "seq_len0",
            0,
            &[],
            "10",
        ),
        case(
            "modules/bif_lists_last",
            "lists:last/1 agrees on a literal list",
            &[BeamAsm, OtpApps],
            "lists_last0",
            0,
            &[],
            "3",
        ),
        case(
            "modules/bif_integer_to_list_length",
            "length/1 of integer_to_list/1 agrees on 12345",
            &[BeamAsm, OtpApps],
            "int_to_list_len0",
            0,
            &[],
            "5",
        ),
        case(
            "modules/recursive_range_sum",
            "a guarded recursive range sum agrees on 1..10",
            &[BeamAsm, LanguageSemantics],
            "sum_range",
            2,
            &[1, 10],
            "55",
        ),
        // --- A.7: the `erl_error:format_exception/3` heap-allocated-reason regression -------
        //
        // `format_exception/3` walks the reason it is handed; a **heap-allocated** reason (the
        // `{1,2}`/`{a,b}` tuples) used to drive the Rust runtime into a `SIGSEGV` while the
        // oracle answered a value. Each case reduces the (deep) formatted charlist to its
        // flattened length — an integer, so the golden is stable — and must agree under both
        // runtimes. `badarg` (an immediate atom) is the neighbouring shape that always worked.
        err_case(
            "modules/erl_error_format_exception_throw_tuple",
            "erl_error:format_exception/3 on a heap-allocated tuple reason (throw, {1,2}) agrees",
            "fmt_throw_tuple",
            "22",
        ),
        err_case(
            "modules/erl_error_format_exception_throw_atom_tuple",
            "erl_error:format_exception/3 on a heap-allocated atom-tuple reason (throw, {a,b}) agrees",
            "fmt_throw_atom_tuple",
            "22",
        ),
        err_case(
            "modules/erl_error_format_exception_error_badarg",
            "erl_error:format_exception/3 on the immediate reason (error, badarg) agrees",
            "fmt_error_badarg",
            "29",
        ),
        // The `run`-action zero-length-binary regression: `base64:encode/1` returns an
        // **off-heap** refc binary even for a zero-length input, and the CLI used to refuse to
        // render it (`beam_rt: badarg`, exit 1) where the oracle printed `<<>>`. The exported
        // function returns the byte size (an integer — a module-run golden must be one), so the
        // case pins the whole `run` path end to end: `0` (this) versus a crash/refusal before.
        case(
            "modules/bif_base64_encode_empty_size",
            "the run action returns a zero-length base64:encode/1 result (byte size 0)",
            &[BeamAsm, OtpApps],
            "b64_empty_size",
            1,
            &[65],
            "0",
        ),
        // --- A.1/A.2: the callee-head `undef` stacktrace item from real bytecode ------------
        //
        // The three-argument `try C:R:St ->` pattern binds the **stacktrace** directly, but the
        // runtime's `-eval` evaluator cannot parse that grammar (`beam_rt: unsupported: …
        // expected an atom, found Some(Var("St"))`), so the shape the `errors/undef_head_item_*`
        // expression cases reach indirectly — through `element(2, element(2, (catch EXPR)))` — is
        // only observable verbatim from a **compiled** module. The innermost item of an `undef`
        // raised by a `call_ext` must be `{Module, Function, ArgList, Location}`: an atom
        // module, an atom function, the real argument **list** and a location list. The golden
        // is the bit mask `is_atom(M) + 2*is_atom(F) + 4*is_list(Args) + 8*is_list(Loc)` plus
        // `100 * length(Args)`, so a 4-tuple with the real 2-element argument list is `215`.
        trace_case(
            "modules/undef_head_item_shape_in_compiled_code",
            "from compiled bytecode the innermost undef stacktrace item has an atom module, atom function and list arg list",
            &[BeamAsm, LanguageSemantics],
            "undef_head_shape",
            "215",
        ),
        // The bit mask alone would accept *any* module/function/argument list, so the identity of
        // the head item is pinned separately: the `call_ext` names `lists:nonexistent_fn_xyz/2`,
        // so the item must read `{lists, nonexistent_fn_xyz, [1,2], _Loc}` (mask `1 + 2 + 4`).
        trace_case(
            "modules/undef_head_item_identity_in_compiled_code",
            "from compiled bytecode the innermost undef stacktrace item names the called module, function and its argument list",
            &[BeamAsm, LanguageSemantics],
            "undef_head_arg_list",
            "7",
        ),
        // --- A.7/A.2: `erlang:fun_info/1` of a fun built by compiled code --------------------
        //
        // `fun ?MODULE:f/0` compiled into the module's own code yields an **external** fun: its
        // `fun_info/1` carries exactly the five `arity`/`env`/`module`/`name`/`type` keys, with
        // the defining module, the exported function's name, arity 0, an empty env and the atom
        // `external` (the `-eval` route reaches only the `fun lists:reverse/1` form, pinned by
        // `system_info.rs`'s `fun_info/external_fun_*` cases). The golden is the five-key list
        // plus all four values, so a full match is `1 + 2 + 4 + 8 + 16 = 31`.
        //
        // The **local**-fun sibling is deliberately absent: `erlang:fun_info/1` of a local fun
        // created by compiled code is an honest capability gap today (the runtime answers
        // `beam_rt: unsupported: bif`, exit 3 — its `fun_info/2` `module` item needs the
        // local-fun identity seam and the whole-list walk needs the loaded module's fun table
        // for the lambda name), so a case pinning it would be an unrecorded gap, never
        // agreement. It stays unpinned until the owning crate (`beam_bifs` / `beam_rt`) closes it.
        trace_case(
            "modules/fun_info_external_fun_created_in_compiled_code",
            "erlang:fun_info/1 of a compiled `fun ?MODULE:f/0` is the five-key external-fun proplist",
            &[BeamAsm, OtpApps],
            "external_fun_info",
            "31",
        ),
        // --- A.7/A.2: the *fired* timer from real bytecode ---------------------------------
        //
        // `timer:apply_after/4` is a plain Erlang library call over `erlang:send_after/3`, so
        // the observable is the same one the `-eval` route pins
        // (`bifs/timer_apply_after_message_is_delivered`): the message must really be sent when
        // the delay elapses. Here the `receive` is **real bytecode** (the `wait`/`timeout`
        // opcodes), which is what makes the case worth a compiled run: the golden is `42` only
        // if the timer fired *and* the engine woke the waiting process; anything else lands in
        // the `after` branch and reads `0`.
        Case {
            id: "modules/timer_apply_after_message_is_delivered_in_compiled_code",
            description: "a compiled module observes the message timer:apply_after/4 sends when the timer fires",
            contracts: &[BeamAsm, OtpApps],
            kind: CaseKind::Module(ModuleRun {
                source: DIFF_TIMER_SOURCE,
                module: "diff_timer",
                function: "wait_fired",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("42"),
                exit_code: 0,
            },
        },
    ]
}
