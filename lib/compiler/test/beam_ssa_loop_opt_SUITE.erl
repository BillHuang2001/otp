%%
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
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
-module(beam_ssa_loop_opt_SUITE).
-include_lib("common_test/include/ct.hrl").
-include("beam_ssa.hrl").
-import_record(beam_ssa_opt, [opt_st]).
-export([all/0, suite/0]).
-export([init_per_suite/1, end_per_suite/1]).
-export([strength_reduction/1, induction_variable_canonicalisation/1,
         strength_reduction_declines/1, strength_reduction_equivalence/1,
         loop_opt_preserves_result/1, kill_switches/1,
         off_by_default/1, non_trivial_loops_unchanged/1]).

suite() -> [{ct_hooks,[ts_install_cth]}, {timetrap,{minutes,5}}].
all() -> [strength_reduction, induction_variable_canonicalisation,
          strength_reduction_declines, strength_reduction_equivalence,
          loop_opt_preserves_result, kill_switches,
          off_by_default, non_trivial_loops_unchanged].
init_per_suite(Config) -> Config.
end_per_suite(_Config) -> ok.

%%=========================================================================
%% Synthetic-SSA tests.
%%
%% The pass runs in the prologue phase, i.e. before the SSA type analysis,
%% and at that point the CFG of a real Erlang function contains no
%% optimizable loop (a self-recursive function is still a tail *call*, not a
%% CFG cycle, and the cycle that a `receive' does create has no canonical
%% induction variable). The rewrites are therefore driven directly on
%% hand-built linear SSA: `beam_ssa_loop_opt:opt/1' is called with the same
%% `#opt_st{}' the pipeline uses, and the result is inspected as a
%% `dssaopt' listing would show it. The *input* block list is what the same
%% function looks like without `loop_opt' (where the pass is not run at
%% all), so "with the pass" is the output and "without the pass" is the
%% input.
%%=========================================================================

strength_reduction(_Config) ->
    %% i = phi(0 outside, i + 1 inside): the entry value and the step are
    %% both non-negative, so i is a non-negative integer on every iteration.
    {In, Out} = run_pass(iv_loop(0, '+', 1)),
    {bif,'*'} = op(In, m),
    {bif,'div'} = op(In, d),
    %% i * 8 -> i bsl 3 and i div 4 -> i bsr 2, rewritten in place: same
    %% destination, same operands, only the operation and the literal change.
    #b_set{op={bif,'bsl'}, dst=#b_var{name=m},
           args=[#b_var{name=i}, #b_literal{val=3}]} = instr(Out, m),
    #b_set{op={bif,'bsr'}, dst=#b_var{name=d},
           args=[#b_var{name=i}, #b_literal{val=2}]} = instr(Out, d),
    %% Everything else in the loop is untouched.
    {bif,'+'} = op(Out, s),

    %% A decreasing induction variable starting at 9 is still a proven
    %% integer, but it can go negative, so only the multiplication may be
    %% reduced; `div' must stay behind because it truncates towards zero.
    {_, Out2} = run_pass(iv_loop(9, '-', 1)),
    {bif,'bsl'} = op(Out2, m),
    {bif,'div'} = op(Out2, d),

    %% `X * 2' is exact for every number -- a float doubles without rounding
    %% -- and becomes `X + X' when X is not known to be an integer; a
    %% product by a larger power of two is left alone in that case.
    {_, OutU} = run_pass(unknown_loop()),
    #b_set{op={bif,'+'}, dst=#b_var{name=m2},
           args=[#b_var{name=x}, #b_var{name=x}]} = instr(OutU, m2),
    {bif,'*'} = op(OutU, m8),
    {bif,'div'} = op(OutU, dv),
    ok.

induction_variable_canonicalisation(_Config) ->
    %% The same loop as above with the step written `1 + i' instead of
    %% `i + 1'.
    {In, Out} = run_pass(iv_loop_lhs(0)),
    %% The step recurrence is canonicalised into `i + 1' ...
    [#b_literal{val=1}, #b_var{name=i}] = args(In, up),
    [#b_var{name=i}, #b_literal{val=1}] = args(Out, up),
    %% ... and the induction variable is recognised, which is what licences
    %% the reduction of the values derived from it.
    #b_set{op={bif,'bsl'}, args=[#b_var{name=i}, #b_literal{val=3}]} =
        instr(Out, m),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=i}, #b_literal{val=2}]} =
        instr(Out, d),

    %% A value computed from an induction variable inherits its facts: with
    %% i >= 0 and step +1 the value `j = i + 1' is a non-negative integer on
    %% every iteration, so both reductions apply to it as well.
    {_, OutD} = run_pass(derived_loop()),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=j}, #b_literal{val=3}]} =
        instr(OutD, jm),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=j}, #b_literal{val=2}]} =
        instr(OutD, jd),
    ok.

strength_reduction_declines(_Config) ->
    %% Literals that are not a power of two, a zero divisor, a divisor of 1
    %% and a non-literal divisor: none of these is rewritten; in particular
    %% `div 0' keeps raising, and no reduction introduces a new exception.
    {_, Out} = run_pass(nonpow2_loop()),
    {bif,'*'} = op(Out, mul3),
    {bif,'div'} = op(Out, div3),
    {bif,'div'} = op(Out, div0),
    {bif,'div'} = op(Out, div1),
    {bif,'div'} = op(Out, divn2),

    %% A non-canonical inside edge (`up = p + i' where `p = n * 8' and n is
    %% itself loop carried) must make `i' *not* an induction variable, so
    %% its arithmetic is left alone. This is the regression test for the
    %% step analysis requiring every inside edge to be a `V +/- C'
    %% recurrence.
    {_, OutN} = run_pass(noncanon_loop()),
    {bif,'*'} = op(OutN, m),
    {bif,'div'} = op(OutN, d),
    ok.

%%=========================================================================
%% Functional equivalence: the same source compiled with and without
%% `loop_opt' must behave identically, including which expressions raise.
%%=========================================================================

strength_reduction_equivalence(Config) ->
    Src = sr_src(),
    Plain = load(Config, sr_plain, Src, []),
    Opt = load(Config, sr_opt, Src, [loop_opt]),
    Vals = [-1000001, -1000, -9, -3, -2, -1, 0, 1, 2, 3, 7, 8, 9, 255, 256,
            1000000, 123456789012345678901234567890,
            2.5, -2.5, 0.0, 1.0e300],
    lists:foreach(
      fun(F) ->
              lists:foreach(
                fun(V) ->
                        A = call(Plain, F, V),
                        B = call(Opt, F, V),
                        case A =:= B of
                            true -> ok;
                            false -> ct:fail({result_mismatch, F, V, A, B})
                        end
                end, Vals)
      end, [mul2, mul8, mul16, mul3, div2, div8, div16, div3,
            div0, div1, idiv2_nonneg, loop_mul]),
    ok.

%% Run `Mod:F(V)' and return the value or the {Class,Reason} of the
%% exception, so that results *and* exceptions can be compared.
call(Mod, F, V) ->
    try {ok, Mod:F(V)} catch C:R -> {C, R} end.

sr_src() ->
    "-module(sr_probe).\n"
    "-export([mul2/1, mul8/1, mul16/1, mul3/1,\n"
    "         div2/1, div8/1, div16/1, div3/1, div0/1, div1/1,\n"
    "         idiv2_nonneg/1, loop_mul/1]).\n"
    "mul2(X) -> X * 2.\n"
    "mul8(X) -> X * 8.\n"
    "mul16(X) -> X * 16.\n"
    "mul3(X) -> X * 3.\n"
    "div2(X) -> X div 2.\n"
    "div8(X) -> X div 8.\n"
    "div16(X) -> X div 16.\n"
    "div3(X) -> X div 3.\n"
    "div0(X) -> X div 0.\n"
    "div1(X) -> X div 1.\n"
    "idiv2_nonneg(X) when is_integer(X), X >= 0 -> X div 2.\n"
    "loop_mul(X) -> loop_mul(X, 20, 0).\n"
    "loop_mul(_X, 0, Acc) -> Acc;\n"
    "loop_mul(X, N, Acc) -> loop_mul(X, N - 1, Acc + X * 8 + X div 4).\n".

%%=========================================================================
%% The pre-existing end-to-end tests.
%%=========================================================================

loop_opt_preserves_result(Config) ->
    Mods = [{lp_count, counter_src()}, {lp_traverse, traverse_src()},
            {lp_nested, nested_src()}, {lp_ctor, invariant_ctor_src()}],
    lists:foreach(
      fun({Name, Src}) ->
              Plain = run(Config, Name, Src, []),
              Opt = run(Config, Name, Src, [loop_opt]),
              case {Plain, Opt} of
                  {R, R} -> ok;
                  _ -> ct:fail({result_mismatch, Name, Plain, Opt})
              end
      end, Mods).

kill_switches(Config) ->
    Src = nested_src(),
    Plain  = run(Config, kill_plain,  Src, []),
    Opt    = run(Config, kill_opt,    Src, [loop_opt]),
    NoLoop = run(Config, kill_noloop, Src, [loop_opt, no_ssa_opt_loop_opt]),
    NoSsa  = run(Config, kill_nossa,  Src, [loop_opt, no_ssa_opt]),
    case {Plain, Opt, NoLoop, NoSsa} of
        {R, R, R, R} -> ok;
        Other -> ct:fail({kill_switch_mismatch, Other})
    end.

off_by_default(Config) ->
    _ = run(Config, default_mod, counter_src(), []),
    Opts = compile:env_compiler_options(),
    case lists:member(loop_opt, Opts) of
        true -> ct:fail(loop_opt_enabled_by_env);
        false -> ok
    end.

non_trivial_loops_unchanged(Config) ->
    Src = raising_src(),
    R1 = (load(Config, lp_raising, Src, [loop_opt])):bad(3),
    R2 = (load(Config, lp_raising_plain, Src, [])):bad(3),
    case {R1, R2} of
        {R, R} when R =:= ok -> ok;
        Other -> ct:fail({raising_mismatch, Other})
    end.

run(Config, Name, Src, Opts) ->
    Mod = load(Config, Name, Src, Opts),
    Mod:sample().

%% Compile `Src' with `Opts', load the resulting module and return its name.
load(Config, Name, Src, Opts) ->
    Priv = ?config(priv_dir, Config),
    Dir = filename:join(Priv, atom_to_list(Name)),
    ok = filelib:ensure_dir(filename:join(Dir, "x")),
    File = filename:join(Dir, atom_to_list(Name) ++ ".erl"),
    ok = file:write_file(File, Src),
    Res = compile:file(File, [binary, return_errors, return_warnings | Opts]),
    {Mod, Bin} = case Res of
                     {ok, M0, B0} -> {M0, B0};
                     {ok, M0, B0, _Ws} -> {M0, B0};
                     _ -> ct:fail({compile_failed, Name, Res})
                 end,
    {module, Mod} = code:load_binary(Mod, File, Bin),
    Mod.

counter_src() ->
    "-module(lp_count).\n-export([sample/0, count/1]).\n"
    "sample() -> [count(N) || N <- lists:seq(0, 20)].\n"
    "count(N) -> count(N, 0).\n"
    "count(0, Acc) -> Acc;\n"
    "count(N, Acc) when N > 0 -> count(N - 1, Acc + N).\n".
traverse_src() ->
    "-module(lp_traverse).\n-export([sample/0, rev/1]).\n"
    "sample() -> [length(rev(lists:seq(1, N))) || N <- lists:seq(0, 50)].\n"
    "rev(L) -> rev(L, []).\n"
    "rev([H | T], Acc) -> rev(T, [H | Acc]);\n"
    "rev([], Acc) -> Acc.\n".
nested_src() ->
    "-module(lp_nested).\n-export([sample/0, outer/1]).\n"
    "sample() -> [outer(N) || N <- lists:seq(0, 12)].\n"
    "outer(N) -> outer(N, 0).\n"
    "outer(0, Acc) -> Acc;\n"
    "outer(N, Acc) when N > 0 -> outer(N - 1, Acc + inner(N, 0)).\n"
    "inner(0, Acc) -> Acc;\n"
    "inner(M, Acc) when M > 0 -> inner(M - 1, Acc + M).\n".
invariant_ctor_src() ->
    "-module(lp_ctor).\n-export([sample/0, f/2]).\n"
    "sample() -> [f(a, N) || N <- lists:seq(0, 25)].\n"
    "f(A, N) -> loop(A, N, 0).\n"
    "loop(_A, 0, Acc) -> Acc;\n"
    "loop(A, N, Acc) when N > 0 ->\n"
    "    T = {A},\n"
    "    A1 = element(1, T),\n"
    "    loop(A1, N - 1, {Acc, A1}).\n".
raising_src() ->
    "-module(lp_raising).\n-export([sample/0, bad/1]).\n"
    "sample() -> bad(3).\n"
    "bad(X) -> bad(X, 0).\n"
    "bad(0, _) -> ok;\n"
    "bad(N, Acc) when N > 0 ->\n"
    "    try bad(N - 1, Acc + N) of R -> R\n"
    "    catch error:E -> {E, ok} end.\n".

%%=========================================================================
%% Synthetic SSA builders and inspectors.
%%=========================================================================

%% Run the pass directly. Returns {Input, Output}, where the input is the
%% SSA as it is produced when `loop_opt' is not given.
run_pass(Linear) ->
    {#opt_st{ssa=Out}, _} =
        beam_ssa_loop_opt:opt({#opt_st{ssa=Linear, args=[], cnt=100, anno=#{}},
                               #{}}),
    {Linear, Out}.

instr(Linear, Name) ->
    Is = lists:append([BlkIs || {_L, #b_blk{is=BlkIs}} <- Linear]),
    case [I || I <- Is, is_dst(I, Name)] of
        [#b_set{}=I] -> I;
        [] -> missing
    end.

op(Linear, Name) ->
    case instr(Linear, Name) of
        #b_set{op=Op} -> Op;
        missing -> missing
    end.

args(Linear, Name) ->
    case instr(Linear, Name) of
        #b_set{args=As} -> As;
        missing -> missing
    end.

is_dst(#b_set{dst=#b_var{name=N}}, N) -> true;
is_dst(_, _) -> false.

var(N) -> #b_var{name=N}.
lit(V) -> #b_literal{val=V}.
set(V, Op, Args) -> #b_set{dst=var(V), op=Op, args=Args}.
bri(L) -> #b_br{bool=#b_literal{val=true}, succ=L, fail=L}.

%% i = phi(Init outside, i +/- Step inside); in the header
%%   m = i * 8,  d = i div 4,  s = m + d
iv_loop(Init, Sign, Step) ->
    Up = case Sign of '+' -> {bif,'+'}; '-' -> {bif,'-'} end,
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(Init), 1}, {var(up), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)]),
                        set(s, {bif,'+'}, [var(m), var(d)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, Up, [var(i), lit(Step)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(s)}}}].

%% The same loop with the step written `1 + i'.
iv_loop_lhs(Init) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(Init), 1}, {var(up), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [lit(1), var(i)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].

%% No definition for `x': it must be treated as unknown.
unknown_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set(m2, {bif,'*'}, [var(x), lit(2)]),
                        set(m8, {bif,'*'}, [var(x), lit(8)]),
                        set(dv, {bif,'div'}, [var(x), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m8)}}}].

%% Divisor/factor edge cases inside a loop with a valid induction variable.
nonpow2_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set(mul3, {bif,'*'}, [var(i), lit(3)]),
                        set(div3, {bif,'div'}, [var(i), lit(3)]),
                        set(div0, {bif,'div'}, [var(i), lit(0)]),
                        set(div1, {bif,'div'}, [var(i), lit(1)]),
                        set(divn2, {bif,'div'}, [lit(2), var(i)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(mul3)}}}].

%% j = i + 1, then j * 8 and j div 4.
derived_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set(j, {bif,'+'}, [var(i), lit(1)]),
                        set(jm, {bif,'*'}, [var(j), lit(8)]),
                        set(jd, {bif,'div'}, [var(j), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(jm)}}}].

%% up = p + i where p = n * 8 and n is itself loop carried: the inside edge
%% of `i' is not a `V +/- C' recurrence.
noncanon_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        #b_set{op=phi, dst=var(n),
                               args=[{lit(1), 1}, {var(np2), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(p, {bif,'*'}, [var(n), lit(8)]),
                       set(up, {bif,'+'}, [var(p), var(i)]),
                       set(np2, {bif,'+'}, [var(n), lit(1)])],
                last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].
