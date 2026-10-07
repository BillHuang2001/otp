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
%% Tests for the opt-in per-SSA-pass time budget, watchdog and eligibility
%% gates implemented by beam_ssa_opt_budget and wired into
%% beam_ssa_opt:passes_1/3 / beam_ssa_destructive_update:opt/2.
%%
%% The machinery is default OFF: when none of the options
%% ssa_opt_budget / {ssa_opt_budget,Millis}, ssa_opt_watchdog /
%% {ssa_opt_watchdog,Millis} or ssa_opt_gates is given, the pass funs are
%% not wrapped at all and no budget state is created (see `default_off/1').
%%
%% Every test case here compiles probe modules with `no_spawn_compiler_process'
%% so that the compilation - and thus the per-compilation statistics
%% (beam_ssa_opt_budget:get_stats/0) - happens in the test process.
%%
-module(ssa_opt_budget_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([default_off/1,
         gates_skip_flat/1,
         gates_run_rich/1,
         gates_do_not_change_output/1,
         budget_ample_does_not_change_output/1,
         budget_takes_effect/1,
         watchdog_takes_effect/1,
         tight_budget_equivalence/1]).

suite() ->
    [{ct_hooks,[ts_install_cth]}].

all() ->
    [{group,p}].

groups() ->
    [{p,[],
      [default_off,
       gates_skip_flat,
       gates_run_rich,
       gates_do_not_change_output,
       budget_ample_does_not_change_output,
       budget_takes_effect,
       watchdog_takes_effect,
       tight_budget_equivalence]}].

init_per_suite(Config) ->
    Config.

end_per_suite(_Config) ->
    ok.

init_per_group(_GroupName, Config) ->
    Config.

end_per_group(_GroupName, Config) ->
    Config.

%%============================================================================
%% Test cases
%%============================================================================

%% Without any of the new options there is no budget state at all, i.e. the
%% machinery is off by default.
default_off(Config) ->
    Builds = build_sets(Config, "ssa_opt_budget_flat", flat_probe(),
                        [{plain, [binary,no_spawn_compiler_process]}]),
    [{plain, {_Mod, Bin}}] = Builds,
    ?assertEqual(undefined, stats()),
    ?assert(is_binary(Bin)),
    ok.

%% The eligibility gates skip the opt-in passes when their (cheap)
%% precondition cannot hold: the flat probe has no loop, no map lookup and
%% no record update, so none of the three passes may run.
gates_skip_flat(Config) ->
    Opts = [binary,no_spawn_compiler_process,ssa_opt_gates,
            loop_opt,fusion_opt,fbip_opt],
    _ = build_sets(Config, "ssa_opt_budget_flat", flat_probe(), [{gates, Opts}]),
    St = stats(),
    ?assert(is_map(St)),
    check(St, ssa_opt_loop_opt, 0, 1),
    check(St, ssa_opt_fus, 0, 1),
    check(St, ssa_opt_fbip, 0, 1),
    ok.

%% ... while the rich probe (a receive loop, a map lookup guard and record
%% updates) opens all three gates.
gates_run_rich(Config) ->
    Opts = [binary,no_spawn_compiler_process,ssa_opt_gates,
            loop_opt,fusion_opt,fbip_opt],
    _ = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), [{gates, Opts}]),
    St = stats(),
    ?assert(is_map(St)),
    check(St, ssa_opt_loop_opt, 1, 0),
    check(St, ssa_opt_fus, 1, 0),
    check(St, ssa_opt_fbip, 1, 0),
    ok.

%% The gates only skip passes that provably cannot fire, so enabling them
%% must not change the generated code at all.
gates_do_not_change_output(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {gates, [binary,no_spawn_compiler_process,ssa_opt_gates,
                     loop_opt,fusion_opt,fbip_opt]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(gates, Builds))),
    ok.

%% A budget that is never reached must not change the generated code
%% either.
budget_ample_does_not_change_output(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {ample, [binary,no_spawn_compiler_process,{ssa_opt_budget,600000}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(ample, Builds))),
    ok.

%% A zero budget is exceeded immediately: the skippable passes stop running
%% and the fixpoint phase is not entered, which is observable in the
%% statistics. The result is less optimized but behaves identically.
budget_takes_effect(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {tight, [binary,no_spawn_compiler_process,{ssa_opt_budget,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    St = stats(),
    ?assert(is_map(St)),
    ?assert(maps:get(over_budget, St) > 0),
    ?assert(code_of(key(plain, Builds)) =/= code_of(key(tight, Builds))),
    check_behaviour(Builds),
    ok.

%% A zero watchdog kills every skippable pass before it can complete; the
%% pass is then treated as "did nothing". Observable in the statistics, and
%% behaviour preserving.
watchdog_takes_effect(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {tight, [binary,no_spawn_compiler_process,{ssa_opt_watchdog,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    St = stats(),
    ?assert(is_map(St)),
    ?assert(maps:get(timed_out, St) > 0),
    ?assert(code_of(key(plain, Builds)) =/= code_of(key(tight, Builds))),
    check_behaviour(Builds),
    ok.

%% Compile the same probe with the default options, with an exhausted
%% budget and with a zero watchdog, execute all three builds and check that
%% they agree exactly (including exceptions).
tight_budget_equivalence(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {budget, [binary,no_spawn_compiler_process,{ssa_opt_budget,0}]},
            {watchdog, [binary,no_spawn_compiler_process,{ssa_opt_watchdog,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    check_behaviour(Builds),
    ok.

%%============================================================================
%% Checking helpers
%%============================================================================

%% Assert that pass `Name' ran at least / at most `Ran' and `Gated' times.
check(Stats, Name, MinRan, MinGated) ->
    Passes = maps:get(passes, Stats, #{}),
    Pass = maps:get(Name, Passes, pass_zero()),
    ?assertEqual({Name, true}, {Name, maps:get(ran, Pass) >= MinRan}),
    ?assertEqual({Name, true}, {Name, maps:get(gated, Pass) >= MinGated}),
    ok.

pass_zero() ->
    #{ran => 0, gated => 0, over_budget => 0, timed_out => 0}.

stats() ->
    beam_ssa_opt_budget:get_stats().

key(Set, Builds) ->
    case lists:keyfind(Set, 1, Builds) of
        {Set, Build} -> Build;
        false -> ct:fail({missing_option_set, Set})
    end.

%% Call every function of every build with the same arguments and check
%% that all builds agree with each other and with the expected results.
check_behaviour(Builds) ->
    lists:foreach(fun({Fun, ArgSets}) ->
                          check_behaviour(Builds, Fun, ArgSets)
                  end, battery()),
    ok.

check_behaviour(Builds, Fun, ArgSets) ->
    Expected = [Value || {_Args,Value} <- ArgSets],
    Results = [{Set, [run(Mod, Bin, Fun, Args) || {Args,_} <- ArgSets]}
               || {Set, {Mod, Bin}} <- Builds],
    lists:foreach(fun({Set, Values}) ->
                          ?assertEqual({Set, Expected}, {Set, Values})
                  end, Results),
    ?assertEqual(1, length(lists:usort([Values || {_Set,Values} <- Results]))),
    ok.

%% Expected result (or exception) for every probe function.
battery() ->
    [{arith, [[1],[-7],[x],[1.5]]},
     {lst, [[],[1,2,3],[[a],[b]],"abc"]},
     {mp, [[#{a => 1}],[#{}],[[{a,1}]],[not_a_map]]},
     {cmp, [[1,2],[2,2],[3,1],[a,b]]},
     {bin, [[<<>>],[<<1,2,3>>],[<<"hello">>]]},
     {rec, [[1],[42],[x]]},
     {rec_loop, [[0],[1],[5]]},
     {ex, [[2],[0],[-4]]}].

%% Run `Mod:Fun(Args)' in the code that was just loaded. The result is
%% `{ok,Value}' for a normal return and `{Class,Reason}' for an exception;
%% the stacktrace is deliberately not part of the comparison.
run(Mod, Bin, Fun, Args) ->
    _ = code:purge(Mod),
    {module, Mod} = code:load_binary(Mod, atom_to_list(Mod) ++ ".erl", Bin),
    try apply(Mod, Fun, Args) of
        Value -> {ok, Value}
    catch
        Class:Reason -> {Class, Reason}
    end.

%%============================================================================
%% Compiling and disassembling the probes
%%============================================================================

build_sets(Config, Mod, Source, Sets) ->
    Dir = tmp_dir(Config),
    File = write_probe(Dir, Mod, Source),
    [{Set, build(File, Opts)} || {Set,Opts} <- Sets].

build(File, Opts) ->
    case compile:file(File, [return_errors,return_warnings|Opts]) of
        {ok, Mod, Bin, []} ->
            {Mod, Bin};
        {ok, _Mod, _Bin, Warnings} ->
            ct:fail({unexpected_warnings, File, Warnings});
        {error, Errors, Warnings} ->
            ct:fail({compile_failed, File, Errors, Warnings})
    end.

%% The code of one build, i.e. the list of
%% `{function,Name,Arity,Label,Is}' entries. The compile_info chunk is not
%% part of it, so builds with different options can be compared directly.
code_of({_Mod, Bin}) ->
    case beam_disasm:file(Bin) of
        {beam_file, _Mod, _Exports, _Attrs, _CompileInfo, Code} -> Code;
        {error, Mod, Reason} -> ct:fail({beam_disasm_failed, Mod, Reason})
    end.

tmp_dir(Config) ->
    Base = case proplists:get_value(priv_dir, Config) of
               undefined ->
                   case os:getenv("TMPDIR") of
                       false -> "/tmp";
                       "" -> "/tmp";
                       EnvDir -> EnvDir
                   end;
               PrivDir ->
                   PrivDir
           end,
    Sub = "ssa_opt_budget_" ++ integer_to_list(erlang:unique_integer([positive])),
    Dir = filename:join(Base, Sub),
    ok = filelib:ensure_path(Dir),
    Dir.

write_probe(Dir, Mod, Source) ->
    File = filename:join(Dir, Mod ++ ".erl"),
    ok = file:write_file(File, Source),
    File.

%%============================================================================
%% Probe modules
%%============================================================================

%% No loop, no map lookup, no record update: every gate must say "skip".
flat_probe() ->
    "-module(ssa_opt_budget_flat).\n"
    "-export([f/1,g/2,h/1]).\n"
    "\n"
    "f(X) when is_integer(X) -> X + 1;\n"
    "f(X) -> X.\n"
    "\n"
    "g(X, Y) -> {X, Y, X =:= Y}.\n"
    "\n"
    "h({a, B}) -> B;\n"
    "h([H | _]) -> H;\n"
    "h(_) -> none.\n".

%% A receive loop, a map lookup guard and record updates: all three gates
%% are open, and there is enough work here to make the watchdog fire when it
%% is given a zero budget.
rich_probe() ->
    "-module(ssa_opt_budget_rich).\n"
    "-export([arith/1,lst/1,mp/1,cmp/2,bin/1,rec/1,rec_loop/1,ex/1,loop/1]).\n"
    "-record(r, {a = 0, b = 0}).\n"
    "\n"
    "arith(X) when is_integer(X) -> X + 1;\n"
    "arith(X) -> X.\n"
    "\n"
    "lst(L) -> [X * 2 || X <- L].\n"
    "\n"
    "mp(M) when is_map(M), is_map_key(a, M) ->\n"
    "    #{a := V} = M,\n"
    "    V;\n"
    "mp(_) -> no.\n"
    "\n"
    "cmp(X, Y) when X =:= Y -> same;\n"
    "cmp(X, Y) when X < Y -> lt;\n"
    "cmp(_, _) -> gt.\n"
    "\n"
    "bin(B) -> <<(byte_size(B)):32, B/binary>>.\n"
    "\n"
    "rec(X) ->\n"
    "    R0 = #r{a = X, b = X + 1},\n"
    "    R1 = R0#r{b = X + 2},\n"
    "    {R1#r.a, R1#r.b}.\n"
    "\n"
    "rec_loop(N) -> rec_loop(N, #r{a = 0}).\n"
    "rec_loop(0, R) -> {R#r.a, R#r.b};\n"
    "rec_loop(N, R) -> rec_loop(N - 1, R#r{a = N}).\n"
    "\n"
    "ex(X) ->\n"
    "    try 1 / X of\n"
    "        V -> V\n"
    "    catch\n"
    "        error:E -> {error, E}\n"
    "    end.\n"
    "\n"
    "loop() ->\n"
    "    receive\n"
    "        {msg, X} -> X\n"
    "    end.\n".
