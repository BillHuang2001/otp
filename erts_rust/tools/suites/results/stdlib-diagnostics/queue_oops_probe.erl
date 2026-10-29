%% %CopyrightBegin%
%%
%% Copyright Ericsson AB 2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% Class: A.1/A.7 language semantics — `erlang:apply/2` is served by an `apply/3`-shaped
%% floor (`crates/beam_rt/src/native.rs:414` -> `apply_dynamic(args,false)`), i.e. it reads
%% `args[0]` as a MODULE and `args[1]` as a FUNCTION and calls `Module:Function()` with zero
%% arguments. So `apply(Fun, Args)` raises `badarg` and `apply(erlang, self)` silently runs
%% `erlang:self/0` — the value disagreement `queue_SUITE:oops` loses to.
%%
%% Run lines (a throwaway cwd; the same argv on both sides):
%%   erlc -o $TMPDIR/stdlib-diag/ebin \
%%        erts_rust/tools/suites/results/stdlib-diagnostics/queue_oops_probe.erl
%%   <rt> -noshell -pa $TMPDIR/stdlib-diag/ebin -eval 'queue_oops_probe:main(), halt(0).'
%%   erl  -noshell -pa $TMPDIR/stdlib-diag/ebin -eval 'queue_oops_probe:main(), halt(0).'
%%
%% Every row runs from COMPILED code: the `-eval` tree-walk evaluator has its own
%% (correct) `apply/2` (`beam_rt/src/eval/mod.rs::apply_2`), so an `-eval`-only probe of
%% `apply/2` measures the evaluator and hides this defect.
-module(queue_oops_probe).
-export([main/0]).

%% -- the smallest compiled reproduction -----------------------------------------------
%% oracle: a          rust: error:badarg
smallest(As) -> apply(fun(X) -> X end, As).

%% -- the suite's shape: `queue_SUITE:call/2`, which turns the raise into a `{diff,…}` ---
%% oracle: {value,a}  rust: {error,badarg}
call(Func, As) ->
    try apply(Func, As) of
	V -> {value,V}
    catch
	C:R -> {C,R}
    end.

%% -- the escalation: two atoms are read as Module:Function() ---------------------------
%% oracle: badarg     rust: the pid of the calling process (a VALUE the oracle rejects)
two_atoms() -> apply(erlang, self).

%% oracle: badarg     rust: undef (it really calls lists:reverse/0)
two_atoms_undef() -> apply(lists, reverse).

%% -- arity mismatch: the reason degrades from {badarity,{Fun,Args}} to badarg -----------
%% oracle: {badarity,{Fun,[a]}}   rust: badarg
%% (Both operands must be non-literal, or the compiler rewrites the call and the row
%% measures `call_fun` instead of `erlang:apply/2`.)
mismatch(F, As) -> apply(F, As).

%% -- an external fun is not special: same floor, same badarg ---------------------------
%% oracle: {badarity,{fun lists:reverse/1,[1,2]}}   rust: badarg
external_fun(As) -> apply(fun lists:reverse/1, As).

%% -- the controls: when the argument list's LENGTH is statically known, the compiler
%% rewrites `apply/2` into `call_fun` (the fun itself need not be known), so the call never
%% reaches `erlang:apply/2` and agrees.
%% oracle: a          rust: a
known_length(F) -> apply(F, [a]).

%% oracle: a          rust: a
literal_args() -> apply(fun(X) -> X end, [a]).

row(Name, F) ->
    io:format("~-44s ~p~n", [Name, (catch F())]).

main() ->
    row("smallest([a])", fun() -> smallest([a]) end),
    row("call(fun(X)->X end, [a])", fun() -> call(fun(X) -> X end, [a]) end),
    row("two_atoms() = apply(erlang, self)", fun() -> two_atoms() end),
    row("two_atoms_undef() = apply(lists, reverse)", fun() -> two_atoms_undef() end),
    row("mismatch(fun()->x end, [a])", fun() -> mismatch(fun() -> x end, [a]) end),
    row("external_fun([1,2])", fun() -> external_fun([1, 2]) end),
    row("known_length(fun(X)->X end) (call_fun)", fun() -> known_length(fun(X) -> X end) end),
    row("literal_args() (call_fun)", fun() -> literal_args() end),
    %% The queue module itself is not in question: an empty queue agrees on both sides.
    row("queue:peek({[],[]})", fun() -> queue:peek({[], []}) end),
    row("queue:len({[],[]})", fun() -> queue:len({[], []}) end),
    row("queue:is_queue({[],[]})", fun() -> queue:is_queue({[], []}) end),
    ok.
