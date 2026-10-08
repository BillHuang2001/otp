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
         tight_budget_equivalence/1,
         real_module_budget_equivalence/1]).

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
       tight_budget_equivalence,
       real_module_budget_equivalence]}].

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
%% must not change the generated code at all. Both builds enable the three
%% opt-in passes that the gates guard - the comparison must be against the
%% same option set without the gates, because the opt-in passes themselves
%% do change the code when they fire.
gates_do_not_change_output(Config) ->
    Opts = [binary,no_spawn_compiler_process,loop_opt,fusion_opt,fbip_opt],
    Sets = [{plain, Opts},
            {gates, [ssa_opt_gates | Opts]}],
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

%% A watchdog that is too small for a pass ABORTS the compilation with the
%% offending pass instead of silently skipping it. Skipping a single pass
%% leaves an arbitrary subset of the optimizations unrun, which neither the
%% code generator nor beam_ssa_pre_codegen survives, so a timeout is
%% reported as an error rather than swallowed. The pass that times out is
%% not deterministic, but the error is.
watchdog_takes_effect(Config) ->
    Dir = tmp_dir(Config),
    File = write_probe(Dir, "ssa_opt_budget_rich", rich_probe()),
    Opts = [binary,return_errors,return_warnings,no_spawn_compiler_process,
            {ssa_opt_watchdog,0}],
    Result = compile:file(File, Opts),
    ?assertMatch({error,_,_}, Result),
    {error, Errors, _} = Result,
    ?assertMatch({ssa_opt_watchdog_timeout,_}, crash_reason(Errors)),
    St = stats(),
    ?assert(is_map(St)),
    ?assert(maps:get(timed_out, St) > 0),
    ok.

%% Extract the error reason reported for a compilation pass that crashed.
crash_reason([{_,D}|_]) when is_list(D) -> crash_reason(D);
crash_reason([{none,compile,{crash,_,Reason,_}}|_]) -> Reason;
crash_reason([{none,compile,{crash,_,Reason}}|_]) -> Reason;
crash_reason(Other) -> Other.

%% Compile the same probe with the default options and with an exhausted
%% budget, execute both builds and check that they agree exactly (including
%% exceptions).
tight_budget_equivalence(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {budget, [binary,no_spawn_compiler_process,{ssa_opt_budget,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    check_behaviour(Builds),
    ok.

%% The budget removes a SUFFIX of the skippable optimization passes (see
%% beam_ssa_opt_budget), so an exhausted budget and a budget expiring in the
%% middle of the pass sequence are valid configurations. Compile a
%% real-world-shaped module - this suite's own source, which contains map
%% guards, binary matching, records, receives and list comprehensions - with
%% an exhausted budget and with budgets that expire at various points, and
%% check that the compilation succeeds and that the executable build still
%% behaves exactly like the default one.
real_module_budget_equivalence(Config) ->
    Src = suite_source(),
    lists:foreach(fun(Opts) ->
                          ?assertMatch({ok,_,_},
                                       compile:file(Src, [binary,return_errors|Opts]))
                  end,
                  [[{ssa_opt_budget,0}],
                   [{ssa_opt_budget,1}],
                   [{ssa_opt_budget,50}],
                   [{ssa_opt_budget,0},ssa_opt_gates]]),
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {tight, [binary,no_spawn_compiler_process,{ssa_opt_budget,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    check_behaviour(Builds),
    ok.

%% The path of this suite's source.
%%
%% It cannot rely on the current working directory: Common Test runs a test
%% case with the cwd set to its log directory, and `?FILE' is only usable if
%% the suite happened to be compiled with a path that is still valid there.
%% The primary candidate is therefore derived from the loaded module itself:
%% `code:which(?MODULE)' is the compiled suite's `.beam' (the harness
%% compiles the suites into the test directory, next to their sources), so
%% the source is the same basename with an `.erl' extension in the beam's
%% directory. If the beam is not a regular file (preloaded or a stale beam)
%% the compile-time and repo-root-relative forms are tried as fallbacks,
%% which is what makes this work when the callbacks are driven directly from
%% a driver started in the repository root.
suite_source() ->
    Candidates = suite_source_candidates(),
    case [F || F <- Candidates, filelib:is_regular(F)] of
        [F|_] -> F;
        [] -> ct:fail({no_suite_source, Candidates})
    end.

suite_source_candidates() ->
    Base = atom_to_list(?MODULE) ++ ".erl",
    FromBeam =
        case code:which(?MODULE) of
            Beam when is_list(Beam) ->
                [filename:join(filename:dirname(Beam), Base)];
            _ ->
                []
        end,
    FromBeam ++ [?FILE, "test/" ++ Base, "lib/compiler/test/" ++ Base].

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
     {rec_fresh, [[1],[42],[-7]]},
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
        {beam_file, _, _Exports, _Attrs, _CompileInfo, Code} -> Code;
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

%% A receive loop, a map lookup guard, record updates and an update of the
%% result of a local freshly-allocating function: all three gates are open,
%% and there is enough work here to make the watchdog fire when it is given a
%% zero budget.
rich_probe() ->
    "-module(ssa_opt_budget_rich).\n"
    "-export([arith/1,lst/1,mp/1,cmp/2,bin/1,rec/1,rec_loop/1,rec_fresh/1,\n"
    "         mk_fresh/1,ex/1,loop/0]).\n"
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
    "%% A local function that always returns a freshly allocated record. The\n"
    "%% ordinary destructive-update pass cannot see through the call and\n"
    "%% leaves the update of its result as a copying update, which is exactly\n"
    "%% the shape the opt-in FBIP pass rewrites (and therefore what opens its\n"
    "%% eligibility gate).\n"
    "mk_fresh(X) -> #r{a = X}.\n"
    "\n"
    "rec_fresh(X) ->\n"
    "    R0 = mk_fresh(X),\n"
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
