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
//! **`erlang:apply/2` is `apply(Fun, Args)`** — the fun/argument-list form, served by the
//! runtime-native floor, oracle-cross-checked on both execution tiers.
//!
//! # Why the floor owns it
//!
//! `apply/2` and `apply/3` are the two "BIFs" ERTS implements as **instructions** rather than
//! as SNIFs (`erts_is_builtin`'s hardcoded special case, `erts/emulator/beam/beam_common.c`;
//! `emu/ops.tab` rewrites a `call_ext*` to them into `i_apply_fun*`), so neither name appears in
//! `bif.tab` and `beam_bifs` carries no implementation for them. The engine special-cases the
//! arity-3 form (`resolve_call_ext_apply`); the arity-2 form reaches the host's `call_bif`, which
//! is why `RuntimeContext::apply_2` (`src/context/apply.rs`) implements it.
//!
//! The wrong reading — `apply(Mod, Fun)` as a zero-arity `Mod:Fun()` — is invisible with a
//! **literal** argument list (the compiler knows the arity and emits `call_fun`/`apply`, never
//! `call_ext`), and only surfaces when the argument list is a **variable** (`beam_ssa_codegen`
//! then emits `call_ext_only 2 {extfunc,erlang,apply,2}`). That is the shape every probe below
//! takes: `Args = id([...])` hides the value from the compiler, so the call really goes through
//! `erlang:apply/2`.
//!
//! # The four failure classes, in the C's order
//!
//! `apply_fun` (`beam_common.c:1734`) decodes the argument list **first**, so a bad list is
//! reported before a non-fun is:
//!
//! 1. a non-list or improper `Args` ⇒ `error:badarg`;
//! 2. more than `MAX_REG - 1` arguments (`decode_arglist`) ⇒ `error:system_limit`;
//! 3. a non-fun `Fun` ⇒ `EXC_BADFUN`, which C's `expand_error_value` wraps into
//!    `{badfun, Fun}` (`beam_common.c:750-772`);
//! 4. a fun applied at the wrong arity ⇒ `{badarity, {Fun, Args}}`, the same expansion.
//!
//! The fixture reports every class through one `caught/2` helper so the answers are values
//! (atoms, booleans, small integers, short tuples) and never a `#Fun<…>` render.
//!
//! # What is pinned, and what is a documented residual
//!
//! `beam_engine` decodes the arity-2 `call_ext` shape in-machine (C's `i_apply_fun*`,
//! `emu/ops.tab:872-874`), so all four classes agree with the oracle: the reason **wrappers**
//! (`{badfun, Fun}`, `{badarity, {Fun, Args}}`) and, for classes 3 and 4, the **caller's** own
//! stacktrace head (C's `non_bif_stacktrace` — the raise comes out of the inner `call_fun`).
//! [`PROBES`] is that equality set and must hold on the default tier and on
//! `BEAM_RT_TIER=interpreter`.
//!
//! [`RESIDUALS`] pins the one shape that still differs: classes 1 and 2 report the oracle's class
//! and reason but a different **stacktrace head**. The oracle arms the BIF item
//! `{erlang, apply, [Fun, Args], [{error_info, #{module => erl_erts_errors}}]}` there, while this
//! engine raises `badarg`/`system_limit` through its non-BIF path and keeps the caller frame. The
//! arming site is `beam_engine`'s raise path, not this crate's; the test pins today's behaviour so
//! the day it closes is a visible, deliberate change to that table.
//!
//! [`APPLIED_RECEIVE`] pins the `receive` inside an applied fun: on **both** routes the nested
//! machine enters in-machine and really parks, so `recv_after/0` waits out its `after 200` and
//! answers `timeout`, exactly like the oracle.
use std::path::Path;
mod support;
/// The banner every loud skip names.
const BANNER: &str = "erlang:apply/2";
/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The fixture: every `apply/2` shape worth distinguishing, in one module.
///
/// Each probe hides its argument list behind `id/1` so the compiler cannot turn the call into a
/// `call_fun`/`apply` instruction — the only way `erlang:apply/2` is reached at all.
///
/// * `a/0`, `c/0`, `zero/0`, `closure/0`, `two_arg/0` — the success path: a closure with one
///   argument, a zero-arity closure, a closure with a **free variable** (`N = 10`), and a
///   two-argument closure (the argument-list decode, not a fixed arity);
/// * `ext/0`, `ext0/0`, `local_mfa/0` — the three **fun kinds** an `apply/2` can be handed: an
///   external `fun lists:reverse/1` (an `EXPORT_EXT`), an external fun naming a runtime-served
///   key (`fun erlang:self/0`) and a local `fun ap2:id/1`;
/// * `cd/0` … `ch/0`, `ctoo/0`, `cok/0`, `carity/0` — the same shapes through `caught/2`, which
///   turns each class into a value and reports the stacktrace head as
///   `{frame, Module, Function, ArityMark, Location}`, where `ArityMark` is
///   `[Fun =:= Fun, Args =:= Args]` when the head is a BIF item (its `A` slot is then the BIF's
///   own `[Fun, Args]` argument vector) and `not_a_list` for a caller frame;
/// * `ce_shape/0`, `carity_shape/0` — the **reason's** shape (does C's `expand_error_value` wrap
///   it?);
/// * `d/0`, `e/0`, `f/0`, `g/0`, `h/0`, `toomany/0` — the uncaught forms, where the runtime's own
///   diagnostic is checked instead of a caught value;
/// * `recv_after/0`, `send_recv/0` — a `receive` **inside** the applied fun.
const FIXTURE_SRC: &str = r#"
-module(ap2).
-export([a/0,c/0,zero/0,closure/0,two_arg/0,ext/0,ext0/0,local_mfa/0,
         d/0,e/0,f/0,g/0,h/0,toomany/0,
         recv_after/0,send_recv/0,
         cd/0,ce/0,cf/0,cg/0,ch/0,ctoo/0,cok/0,carity/0,
         ce_shape/0,carity_shape/0,id/1]).
%% The compiler barrier: a variable argument list is what makes the call site a
%% `call_ext_only 2 {extfunc,erlang,apply,2}` instead of an `apply`/`call_fun` instruction.
id(X) -> X.
a() -> F = fun(X) -> X+1 end, Args = id([1]), apply(F, Args).
c() -> F = fun() -> {[]} end, Args = id([]), apply(F, Args).
zero() -> F = fun() -> 7 end, Args = id([]), apply(F, Args).
closure() -> N = 10, F = fun(X) -> X+N end, Args = id([5]), apply(F, Args).
two_arg() -> F = fun(X, Y) -> X*Y end, Args = id([6, 7]), apply(F, Args).
%% The fun kinds: an `EXPORT_EXT`, one naming a runtime-served key, and a local `fun M:F/A`.
ext() -> F = fun lists:reverse/1, Args = id([[1,2,3]]), apply(F, Args).
ext0() -> F = fun erlang:self/0, Args = id([]), is_pid(apply(F, Args)).
local_mfa() -> F = fun ap2:id/1, Args = id([9]), apply(F, Args).
d() -> erlang:apply(erlang, self).
e() -> F = notafun, Args = id([1]), apply(F, Args).
f() -> F = fun(X) -> X end, Args = id([1|2]), apply(F, Args).
g() -> F = fun(X) -> X end, Args = id(notalist), apply(F, Args).
h() -> erlang:apply(lists, reverse).
toomany() -> F = fun(X) -> X end, Args = id(lists:seq(1, 2000)), apply(F, Args).

%% One `try … catch` around the apply, reporting the class, the reason's class and the head of
%% the stacktrace as a comparable shape (never a fun).
caught(Fun, Args) ->
    try apply(Fun, Args) of
        V -> {value, V}
    catch C:R:S -> {C, reason_class(R), head(S, Fun, Args)}
    end.
cd() -> caught(erlang, self).
ce() -> caught(notafun, id([1])).
cf() -> F = fun(X) -> X end, caught(F, id([1|2])).
cg() -> F = fun(X) -> X end, caught(F, id(notalist)).
ch() -> caught(lists, reverse).
ctoo() -> F = fun(X) -> X end, caught(F, id(lists:seq(1, 2000))).
cok() -> F = fun(X) -> X+1 end, caught(F, id([41])).
carity() -> F = fun(X) -> X end, caught(F, id([1,2])).
ce_shape() -> reason_shape(raw(notafun, id([1]))).
carity_shape() -> reason_shape(raw(fun(X) -> X end, id([1,2]))).
raw(Fun, Args) ->
    try apply(Fun, Args) of V -> {value, V} catch C:R -> {C, R} end.
%% `{badfun, Fun}` / `{badarity, {Fun, Args}}` is C's `expand_error_value`; a bare atom is not.
reason_shape({error, {badfun, F}}) -> {error, {badfun, is_atom(F)}};
reason_shape({error, {badarity, {F, A}}}) -> {error, {badarity, is_function(F), is_list(A)}};
reason_shape(R) -> R.

reason_class({badfun, _}) -> badfun;
reason_class({badarity, _}) -> badarity;
reason_class(R) -> R.
loc([]) -> none;
loc([{error_info, #{module := M}}]) -> {error_info, M};
loc(Other) when is_list(Other) -> {other, length(Other)}.
head([{M, F, A, L} | _], Fun, Args) -> {frame, M, F, arity(A, Fun, Args), loc(L)};
head(Other, _, _) -> {other, length(Other)}.
arity(A, Fun, Args) when is_list(A) ->
    case A of
        [Fun0, Args0] -> [Fun0 =:= Fun, Args0 =:= Args];
        _ -> {length, length(A)}
    end;
arity(_, _, _) -> not_a_list.

recv_after() ->
    F = fun() -> receive X -> X after 200 -> timeout end end,
    Args = id([]),
    apply(F, Args).
send_recv() ->
    F = fun() -> receive X -> {got, X} after 500 -> timeout end end,
    self() ! hello,
    Args = id([]),
    apply(F, Args).
"#;
/// The `apply/2` shapes that must agree with the oracle **exactly**, on every tier.
///
/// The strings are the oracle's own `~1000p` rendering (`ap2:<probe>()` under OTP 29.1.2 /
/// `erts-17.1.1`) and are re-measured against it on every run, so a drift in the oracle is a
/// failure here rather than a silently stale golden.
const PROBES: [(&str, &str); 14] = [
    // The success path: one argument, zero arguments, a free variable, two arguments.
    ("a", "2"),
    ("c", "{[]}"),
    ("zero", "7"),
    ("closure", "15"),
    ("two_arg", "42"),
    // Every fun kind an `apply/2` can be handed: an `EXPORT_EXT`, a runtime-served key, a
    // local `fun M:F/A`.
    ("ext", "[3,2,1]"),
    ("ext0", "true"),
    ("local_mfa", "9"),
    // A non-fun target (`EXC_BADFUN`) and a fun applied at the wrong arity
    // (`EXC_BADARITY`) come out of the inner `call_fun`, so their stacktrace head is the
    // **caller's** frame — `{ap2, caught, not_a_list, {other, 2}}`: C's `non_bif_stacktrace`,
    // where the arity slot is not a list (the raise names no argument vector).
    ("ce", "{error,badfun,{frame,ap2,caught,not_a_list,{other,2}}}"),
    ("carity", "{error,badarity,{frame,ap2,caught,not_a_list,{other,2}}}"),
    // …and C's `expand_error_value` wraps the reason: `{badfun, Fun}` / `{badarity, {Fun, Args}}`.
    ("ce_shape", "{error,{badfun,true}}"),
    ("carity_shape", "{error,{badarity,true,true}}"),
    // The control: a served key that really applies.
    ("cok", "{value,42}"),
    // A `receive` whose message is already in the mailbox never parks, so a nested machine
    // answers it on the scheduled route too.
    ("send_recv", "{got,hello}"),
];
/// The shapes this runtime does **not** yet reproduce, pinned as they are measured.
///
/// Each entry is `(probe, oracle, this runtime)`, and every one of them is the **same** engine
/// gap: `apply_fun`'s own argument-list decode (`badarg` for a non-list/improper list,
/// `system_limit` past the register-file bound) reports the oracle's class and reason but a
/// different stacktrace head. The oracle arms the BIF item
/// `{erlang, apply, [Fun, Args], [{error_info, #{module => erl_erts_errors}}]}`
/// (`erts_engine_error` — the `[true,true]` arity mark says the item's `A` slot really is the
/// BIF's own `[Fun, Args]` argument vector); this runtime raises those two classes through the
/// non-BIF path, which keeps the **caller** frame. Arming the BIF item is `beam_engine`'s, so
/// the expectations below are what this crate can honestly claim today; when the engine lands it
/// these rows become equalities and this table is what says so.
const RESIDUALS: [(&str, &str, &str); 5] = [
    (
        "cd",
        "{error,badarg,{frame,erlang,apply,[true,true],{error_info,erl_erts_errors}}}",
        "{error,badarg,{frame,ap2,caught,not_a_list,{other,2}}}",
    ),
    (
        "cf",
        "{error,badarg,{frame,erlang,apply,[true,true],{error_info,erl_erts_errors}}}",
        "{error,badarg,{frame,ap2,caught,not_a_list,{other,2}}}",
    ),
    (
        "cg",
        "{error,badarg,{frame,erlang,apply,[true,true],{error_info,erl_erts_errors}}}",
        "{error,badarg,{frame,ap2,caught,not_a_list,{other,2}}}",
    ),
    (
        "ch",
        "{error,badarg,{frame,erlang,apply,[true,true],{error_info,erl_erts_errors}}}",
        "{error,badarg,{frame,ap2,caught,not_a_list,{other,2}}}",
    ),
    (
        "ctoo",
        "{error,system_limit,{frame,erlang,apply,[true,true],{error_info,erl_erts_errors}}}",
        "{error,system_limit,{frame,ap2,caught,not_a_list,{other,2}}}",
    ),
];
/// The uncaught probes and the reason atom each must name in the runtime's own diagnostic (the
/// oracle's equivalent observable is a `catch` value — these are the same classes, uncaught).
const UNCAUGHT: [(&str, &str); 6] = [
    ("d", "badarg"),
    ("e", "badfun"),
    ("f", "badarg"),
    ("g", "badarg"),
    ("h", "badarg"),
    ("toomany", "system_limit"),
];
/// The execution tiers the probes run on: the process **default** (the variable is removed, so an
/// ambient `BEAM_RT_TIER` cannot leak in) and the `BEAM_RT_TIER=interpreter` reference tier.
const TIERS: [(Option<&str>, &str); 2] = [(None, "default"), (Some("interpreter"), "interpreter")];
/// One bounded child run on one tier.
fn run_tier(args: &[&str], tier: Option<&str>, what: &str) -> support::Observed {
    let output = support::run_bounded_with_env(
        Path::new(BINARY),
        args,
        &[("BEAM_RT_TIER", tier)],
    )
    .unwrap_or_else(|| panic!("`{what}` could not be run to completion (killed at the bound)"));
    support::Observed {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}
/// `beam_rt run ap2.beam ap2:<function>/0` — the **scheduled** route (a real process).
fn run_scheduled(
    fixture: &support::ErlangFixture,
    function: &str,
    tier: Option<&str>,
) -> support::Observed {
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let beam = fixture.beam();
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("ap2:{function}/0");
    run_tier(
        &["-noshell", "-pa", dir, "run", beam, &spec],
        tier,
        function,
    )
}
/// `beam_rt -noshell -pa <dir> -eval 'io:format("~1000p~n", [ap2:<function>()]).'` — the
/// **pumping** route, where a nested run may spend the timer heap and the mailbox.
///
/// `~1000p` is `~p` at a 1000-column width, so a nested term stays one line in both runtimes (the
/// CLI's own `run` route prints `~w`, which never wraps; the oracle needs the wider `~p` to
/// match it).
fn run_eval(
    fixture: &support::ErlangFixture,
    expression: &str,
    tier: Option<&str>,
) -> support::Observed {
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let expr = format!("io:format(\"~1000p~n\",[{expression}]), halt(0).");
    run_tier(&["-noshell", "-pa", dir, "-eval", &expr], tier, expression)
}
/// **`apply(Fun, Args)` answers the oracle's value, `badarg` and `system_limit` shapes exactly.**
///
/// The oracle is probed first and must print every recorded answer — that is what keeps the
/// goldens pinned to the system ERTS instead of to this suite's memory. Then each tier runs the
/// same probes on the **scheduled** route (`beam_rt run`), which is the route a real program
/// takes, and must reproduce every answer byte for byte.
///
/// The probes are shaped so the compiler cannot elide the `erlang:apply/2` call site: a literal
/// argument list becomes a `call_fun` instruction and never reaches the floor.
///
/// Skips **loudly** without `erl`/`erlc`.
#[test]
fn apply_of_two_arguments_answers_the_oracle_on_both_tiers() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "apply_two", "ap2", FIXTURE_SRC)
    else {
        return;
    };
    // The oracle is the spec.
    for (function, expected) in PROBES {
        let answer = support::fixture_oracle_answer(BANNER, fixture.dir(), &format!("ap2:{function}()"))
            .unwrap_or_else(|| panic!("the oracle could not answer ap2:{function}/0"));
        assert_eq!(answer, expected, "ap2:{function}/0 under the oracle");
    }
    for (tier, label) in TIERS {
        for (function, expected) in PROBES {
            let run = run_scheduled(&fixture, function, tier);
            // A **caught** raise is a value, so the process returns normally whatever the class
            // was: a non-zero exit here means the raise escaped, i.e. the wrong class.
            assert_eq!(
                run.code,
                0,
                "{label}: ap2:{function}/0 must return normally; stdout={:?} stderr={:?}",
                run.stdout.trim(),
                run.stderr.trim()
            );
            assert!(
                !run.stderr.contains("unsupported"),
                "{label}: ap2:{function}/0 must not be a capability gap; stderr={:?}",
                run.stderr.trim()
            );
            assert_eq!(
                run.stdout.trim(),
                expected,
                "{label}: ap2:{function}/0 must answer the oracle's {expected:?}"
            );
        }
    }
}
/// **The `apply_fun` argument-decode classes keep the oracle's class, with the head residual
/// pinned.**
///
/// `cd` (a pid where the argument list belongs), `cf` (an improper list), `cg` (a bare atom),
/// `ch` (a module atom) and `ctoo` (past the register-file bound) must report the same
/// `error`/`badarg`/`system_limit` classification as the oracle — and they do. What differs today
/// is *where* the report comes from: the oracle arms the BIF item
/// `{erlang, apply, [Fun, Args], [{error_info, #{module => erl_erts_errors}}]}`, this runtime
/// raises through the non-BIF path and keeps the caller's frame
/// (`{ap2, caught, not_a_list, {other, 2}}`).
///
/// The two classes that used to sit in this table are gone: `ce` (a non-fun) and `carity` (the
/// wrong arity) now reproduce the oracle **exactly**, reason wrapper
/// (`{badfun, Fun}` / `{badarity, {Fun, Args}}`) and caller frame included, and they are pinned as
/// equalities in [`PROBES`].
///
/// The remaining difference is the engine's raise path, not this crate's; pinning it here keeps
/// the claim honest and makes the day it is fixed a visible, deliberate change to this table.
#[test]
fn the_argument_decode_classes_pin_their_one_head_item_residual() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "apply_two_res", "ap2", FIXTURE_SRC)
    else {
        return;
    };
    for (tier, label) in TIERS {
        for (function, oracle, rust) in RESIDUALS {
            let answer = support::fixture_oracle_answer(BANNER, fixture.dir(), &format!("ap2:{function}()"))
                .unwrap_or_else(|| panic!("the oracle could not answer ap2:{function}/0"));
            assert_eq!(answer, oracle, "ap2:{function}/0 under the oracle");
            let run = run_scheduled(&fixture, function, tier);
            assert_eq!(
                run.code, 0,
                "{label}: ap2:{function}/0 must return normally; stderr={:?}",
                run.stderr.trim()
            );
            assert_eq!(
                run.stdout.trim(),
                rust,
                "{label}: ap2:{function}/0 is the pinned residual (a change here is the engine's \
                 call_ext raise path landing, and this table must then become an equality with \
                 the oracle's {oracle:?})"
            );
            // The exception class itself already agrees, on both sides.
            assert!(
                oracle.starts_with("{error,") && run.stdout.trim().starts_with("{error,"),
                "{label}: ap2:{function}/0 must be an `error`-class raise on both sides; \
                 oracle={oracle:?} rust={:?}",
                run.stdout.trim()
            );
        }
    }
}
/// **An uncaught `apply/2` failure is a genuine runtime error naming the oracle's reason atom.**
///
/// The caught probes above report the classes as values; these are the same seven shapes
/// **uncaught**, where the observable is the runtime's own diagnostic: exit 1 (a real runtime
/// error — never the `3` a capability gap exits with, and never the `0` a wrong `erlang:self/0`
/// call would exit with), empty stdout (the failing apply returns no value), and a stderr line
/// that names the reason atom the oracle also raises.
///
/// `d` is the probe that pins the fix's own regression: `erlang:apply(erlang, self)` read the
/// first argument as a **module** and called `erlang:self/0` instead of rejecting the atom
/// `erlang` as a non-fun.
#[test]
fn an_uncaught_apply_two_failure_names_the_oracles_reason_atom() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "apply_two_uncaught", "ap2", FIXTURE_SRC)
    else {
        return;
    };
    for (tier, label) in TIERS {
        for (function, class) in UNCAUGHT {
            let run = run_scheduled(&fixture, function, tier);
            assert_eq!(
                run.code, 1,
                "{label}: an uncaught ap2:{function}/0 must be a runtime error (exit 1); \
                 stdout={:?} stderr={:?}",
                run.stdout.trim(),
                run.stderr.trim()
            );
            assert!(
                run.stdout.trim().is_empty(),
                "{label}: ap2:{function}/0 raised, so it must print no value; got {:?}",
                run.stdout.trim()
            );
            assert!(
                run.stderr.contains(class),
                "{label}: ap2:{function}/0 must report `{class}`; stderr={:?}",
                run.stderr.trim()
            );
            assert!(
                !run.stderr.contains("unsupported"),
                "{label}: ap2:{function}/0 must not be a capability gap; stderr={:?}",
                run.stderr.trim()
            );
        }
    }
}
/// **A `receive` inside the applied fun parks on both routes.**
///
/// `apply/2` enters the closure **in-machine** on every route, so a `receive` that has to wait
/// suspends the process for real instead of folding into a `blocked` verdict:
///
/// * the `-eval` route pumps (`RECEIVE_PUMP_SLICES` + one port round + the timer heap), so
///   `recv_after/0` waits out its `after 200` and answers `timeout` — the oracle's value;
/// * the **scheduled** route (`beam_rt run`) answers the same `timeout` on both tiers: the
///   nested machine parks the scheduled process and a later pass resumes it;
/// * `send_recv/0` is the control that shows the parking is not unconditional — a message
///   already in the mailbox is answered `{got,hello}` on both routes.
#[test]
fn a_receive_inside_the_applied_fun_parks_on_both_routes() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "apply_two_recv", "ap2", FIXTURE_SRC)
    else {
        return;
    };
    for (tier, label) in TIERS {
        // Every route reproduces the oracle, waiting included.
        let oracle = support::fixture_oracle_answer(BANNER, fixture.dir(), "ap2:recv_after()")
            .unwrap_or_else(|| panic!("the oracle could not answer ap2:recv_after/0"));
        assert_eq!(oracle, "timeout", "ap2:recv_after/0 under the oracle");
        let pumped = run_eval(&fixture, "ap2:recv_after()", tier);
        assert_eq!(
            pumped.code, 0,
            "{label}: the `-eval` route must pump the applied fun's receive; stderr={:?}",
            pumped.stderr.trim()
        );
        assert_eq!(
            pumped.stdout.trim(),
            oracle,
            "{label}: ap2:recv_after/0 on the `-eval` route must answer the oracle's value"
        );
        // A message already in the mailbox is answered on either route.
        let scheduled = run_scheduled(&fixture, "send_recv", tier);
        assert_eq!(
            scheduled.code, 0,
            "{label}: ap2:send_recv/0 must not park; stderr={:?}",
            scheduled.stderr.trim()
        );
        assert_eq!(
            scheduled.stdout.trim(),
            "{got,hello}",
            "{label}: ap2:send_recv/0 on the scheduled route must answer the oracle's value"
        );
        // A receive that must wait inside the nested machine parks the scheduled process and
        // takes its timeout, like the oracle.
        let parked = run_scheduled(&fixture, "recv_after", tier);
        assert_eq!(
            parked.code, 0,
            "{label}: ap2:recv_after/0 must park the scheduled process and answer, not fail; \
             stdout={:?} stderr={:?}",
            parked.stdout.trim(),
            parked.stderr.trim()
        );
        assert_eq!(
            parked.stdout.trim(),
            oracle,
            "{label}: ap2:recv_after/0 on the scheduled route must answer the oracle's value"
        );
        assert!(
            !parked.stderr.contains("unsupported"),
            "{label}: ap2:recv_after/0 must not be a capability gap; stderr={:?}",
            parked.stderr.trim()
        );
    }
}
