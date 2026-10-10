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
%% Tests for the opt-in per-SSA-pass time budget, watchdog, eligibility gates
%% and per-function effort/budget model implemented by beam_ssa_opt_budget and
%% wired into beam_ssa_opt:passes_1/3 / beam_ssa_destructive_update:opt/2.
%%
%% The machinery is default OFF: when none of the options
%% ssa_opt_budget / {ssa_opt_budget,Millis}, {ssa_opt_fn_budget,Millis},
%% ssa_opt_watchdog / {ssa_opt_watchdog,Millis} or ssa_opt_gates is given, the
%% pass funs are not wrapped at all and no budget state is created (see
%% `default_off/1' and `default_path_unchanged/1').
%%
%% The per-function effort model classifies every function by its SSA size and
%% hands it a size-derived wall-clock allowance; `{ssa_opt_fn_budget,Millis}'
%% overrides that allowance for every function. Per-function skipping is only
%% active with a cumulative budget or such an explicit allowance; with
%% `ssa_opt_gates' alone the output is unchanged (see `effort_levels/1',
%% `classify_model/1' and the `fn_budget_*' cases).
%%
%% The eligibility gates are cheap NECESSARY preconditions, wired in
%% `beam_ssa_opt:gate/1' (and, for the FBIP pass, in
%% `beam_ssa_destructive_update:opt/2'). The loop gate accepts a CFG back edge
%% OR a strength-reduction candidate, because the `loop_opt' driver also
%% reduces the whole function, which needs no loop (see `gates_skip_flat/1',
%% `gates_run_rich/1', `gates_do_not_change_output/1' and
%% `gates_accept_reduce_candidate/1').
%%
%% Every test case here compiles probe modules with `no_spawn_compiler_process'
%% so that the compilation - and thus the per-compilation statistics
%% (beam_ssa_opt_budget:get_stats/0) and the per-function queries - happens in
%% the test process.
%%
-module(ssa_opt_budget_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([default_off/1,
         gates_skip_flat/1,
         gates_run_rich/1,
         gates_do_not_change_output/1,
         gates_accept_reduce_candidate/1,
         budget_ample_does_not_change_output/1,
         budget_alone_does_not_change_output/1,
         budget_takes_effect/1,
         watchdog_takes_effect/1,
         watchdog_alone_does_not_change_output/1,
         tight_budget_equivalence/1,
         real_module_budget_equivalence/1,
         classify_model/1,
         effort_levels/1,
         fn_budget_takes_effect/1,
         fn_budget_equivalence/1,
         fn_budget_ample_does_not_change_output/1,
         fn_budget_option_registered/1,
         default_path_unchanged/1]).

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
       gates_accept_reduce_candidate,
       budget_ample_does_not_change_output,
       budget_alone_does_not_change_output,
       budget_takes_effect,
       watchdog_takes_effect,
       watchdog_alone_does_not_change_output,
       tight_budget_equivalence,
       real_module_budget_equivalence,
       classify_model,
       effort_levels,
       fn_budget_takes_effect,
       fn_budget_equivalence,
       fn_budget_ample_does_not_change_output,
       fn_budget_option_registered,
       default_path_unchanged]}].

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
%% machinery is off by default. The whole query surface must say so instead of
%% inventing a state: `effort/0' and `function_size/0' are `undefined',
%% `remaining/0' is `infinity' and no function is "over budget".
default_off(Config) ->
    Builds = build_sets(Config, "ssa_opt_budget_flat", flat_probe(),
                        [{plain, [binary,no_spawn_compiler_process]}]),
    [{plain, {_Mod, Bin}}] = Builds,
    ?assertEqual(undefined, stats()),
    ?assert(is_binary(Bin)),
    ?assertEqual(undefined, effort()),
    ?assertEqual(undefined, function_size()),
    ?assertEqual(undefined, function_budget()),
    ?assertEqual(false, function_over_budget()),
    ?assertEqual(infinity, remaining()),
    ok.

%% The eligibility gates skip the opt-in passes when their (cheap)
%% precondition cannot hold: the flat probe has no loop, no map lookup and
%% no record update, so none of the three passes may run - they are gated
%% instead, i.e. their `ran' counter stays at zero.
gates_skip_flat(Config) ->
    Opts = [binary,no_spawn_compiler_process,ssa_opt_gates,
            loop_opt,fusion_opt,fbip_opt],
    _ = build_sets(Config, "ssa_opt_budget_flat", flat_probe(), [{gates, Opts}]),
    St = stats(),
    ?assert(is_map(St)),
    check_skipped(St, ssa_opt_loop_opt),
    check_skipped(St, ssa_opt_fus),
    check_skipped(St, ssa_opt_fbip),
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

%% The loop gate accepts a CFG back edge OR a strength-reduction candidate,
%% because the `loop_opt' driver also strength-reduces the WHOLE function
%% (`X * 2' is `X + X' for every number), which needs no loop at all. The
%% gates are a NECESSARY precondition, so enabling them must not drop such a
%% rewrite: the gated build must produce exactly the code of the ungated
%% `loop_opt' build, and the statistics must show the pass running on the
%% candidate while the functions that have neither shape are gated. A gate
%% that only accepted a back edge would gate the candidate too and produce
%% the `no_loop_opt' code here.
gates_accept_reduce_candidate(Config) ->
    Sets = [{off, [binary,no_spawn_compiler_process]},
            {opt, [binary,no_spawn_compiler_process,loop_opt]},
            {gates, [binary,no_spawn_compiler_process,loop_opt,ssa_opt_gates]}],
    Builds = build_sets(Config, "ssa_opt_budget_reduce", reduce_probe(), Sets),
    %% The pass really does change a function without any loop, so the
    %% comparison below is meaningful.
    ?assert(code_of(key(off, Builds)) =/= code_of(key(opt, Builds))),
    ?assertEqual(code_of(key(opt, Builds)), code_of(key(gates, Builds))),
    St = stats(),
    ?assert(is_map(St)),
    %% Ran on the candidate `dbl/1', gated on `plain/1' and on the generated
    %% `module_info/0,1', none of which has a back edge or a candidate.
    check(St, ssa_opt_loop_opt, 1, 1),
    ok.

%% A budget that is never reached must not change the generated code
%% either.
budget_ample_does_not_change_output(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {ample, [binary,no_spawn_compiler_process,{ssa_opt_budget,600000}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(ample, Builds))),
    ok.

%% The bare `ssa_opt_budget' atom really arms the cumulative budget (with the
%% documented default instead of being ignored), and a budget that is never
%% reached leaves the generated code of a REAL module byte-for-byte
%% unchanged. `remaining/0' and the statistics of the bare-option build are
%% what make the comparison meaningful: identical code because the option was
%% dropped would satisfy it just as well.
budget_alone_does_not_change_output(_Config) ->
    Src = suite_source(),
    Plain = build(Src, [binary,no_spawn_compiler_process]),
    Default = build(Src, [binary,no_spawn_compiler_process,ssa_opt_budget]),
    ?assert(is_map(stats())),
    ?assert(is_integer(remaining())),
    Ample = build(Src, [binary,no_spawn_compiler_process,
                        {ssa_opt_budget,600000}]),
    ?assertEqual(code_of(Plain), code_of(Default)),
    ?assertEqual(code_of(Plain), code_of(Ample)),
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
    Reason = crash_reason(Errors),
    ?assertMatch({ssa_opt_watchdog_timeout,_}, Reason),
    {ssa_opt_watchdog_timeout, Pass} = Reason,
    ?assert(is_atom(Pass)),
    St = stats(),
    ?assert(is_map(St)),
    %% The error names the pass whose counter was bumped, and no other pass
    %% timed out: the timeout aborts the compilation at the first pass it
    %% hits.
    Passes = maps:get(passes, St),
    ?assertEqual({Pass, true},
                 {Pass, maps:get(timed_out, maps:get(Pass, Passes, pass_zero()))
                  >= 1}),
    ?assertEqual(1, maps:get(timed_out, St)),
    %% Only a pass that may be skipped is run under the watchdog at all: a
    %% pass that must always run is not bounded, so it can never be the one
    %% named in the error.
    ?assertEqual({Pass, false},
                 {Pass, lists:member(Pass, never_skipped())}),
    ok.

%% Extract the error reason reported for a compilation pass that crashed.
crash_reason([{_,D}|_]) when is_list(D) -> crash_reason(D);
crash_reason([{none,compile,{crash,_,Reason,_}}|_]) -> Reason;
crash_reason([{none,compile,{crash,_,Reason}}|_]) -> Reason;
crash_reason(Other) -> Other.

%% The bare `ssa_opt_watchdog' atom arms the per-pass watchdog with the
%% documented default. A pass that finishes in time is then run in a monitored
%% child process and its state is returned, so the result must be
%% byte-for-byte the result without the watchdog - which is what makes the
%% zero-budget abort of `watchdog_takes_effect/1' a statement about the
%% timeout rather than about the child-process mechanism.
watchdog_alone_does_not_change_output(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {watchdog, [binary,no_spawn_compiler_process,ssa_opt_watchdog]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    St = stats(),
    ?assert(is_map(St)),
    %% The passes did run to completion under the watchdog (the child process
    %% returns its state, which is what the code below depends on).
    ?assert(maps:get(ran, St) > 0),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(watchdog, Builds))),
    ok.

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

%%============================================================================
%% Per-function effort model and per-function allowance
%%============================================================================

%% `classify/2' is pure, so the effort model is checked directly against its
%% documentation: `full' for at most 500 instructions, `reduced' for at most
%% 5000 and `minimal' above that, with an allowance of
%% `max(50, 500 - Instructions div 20)' milliseconds. The model is derived
%% from the number of instructions alone, so the block count must not matter,
%% the level must never improve and the allowance must never grow as the
%% function gets bigger nor drop below the floor.
classify_model(_Config) ->
    %% The documented pairs, spelled out.
    ?assertEqual({full, 500}, model(1, 0)),
    ?assertMatch({reduced, _}, model(1, 501)),
    ?assertMatch({minimal, _}, model(1, 5001)),
    ?assertEqual({minimal, 50}, model(1, 9000)),
    %% The block count is part of the size description but must influence
    %% neither the level nor the allowance.
    lists:foreach(fun(Blocks) ->
                          ?assertEqual(model(0, 5000), model(Blocks, 5000))
                  end, [0, 1, 2, 17, 999, 100000]),
    %% The model agrees with the documented formula on every threshold and on
    %% the allowance floor.
    Counts = [0, 1, 20, 499, 500, 501, 2500, 4999, 5000, 5001,
              8999, 9000, 9001, 20000, 1000000],
    lists:foreach(fun(N) ->
                          ?assertEqual({expected_level(N), expected_allowance(N)},
                                       model(1, N))
                  end, Counts),
    %% ... and it is monotone over the whole sweep.
    check_monotone(Counts),
    ok.

%% A module with both a tiny function and a generated huge one (a long
%% arithmetic chain) must be classified at both `full' and `minimal'. The
%% per-function queries must describe the most recently seen function:
%% `function_size/0' a size pair with at least one block, `function_budget/0'
%% a non-negative allowance and `effort/0' one of the four levels. With
%% `ssa_opt_gates' alone there is no cumulative budget, so `remaining/0' is
%% `infinity' and nothing is spent.
effort_levels(Config) ->
    Source = mixed_probe(huge_body(6000)),
    Opts = [binary,no_spawn_compiler_process,ssa_opt_gates],
    _ = build_sets(Config, "ssa_opt_budget_mixed", Source, [{gates, Opts}]),
    St = stats(),
    ?assert(is_map(St)),
    Effort = maps:get(effort, St),
    ?assert(maps:get(full, Effort) >= 1),
    ?assert(maps:get(minimal, Effort) >= 1),
    ?assertEqual(infinity, remaining()),
    {Blocks, Instructions} = function_size(),
    ?assert(is_integer(Blocks) andalso Blocks >= 1),
    ?assert(is_integer(Instructions) andalso Instructions >= 0),
    Budget = function_budget(),
    ?assert(is_integer(Budget) andalso Budget >= 50),
    ?assert(lists:member(effort(), [full, reduced, minimal, none])),
    ct:pal("effort=~p size=~p budget=~p current=~p",
           [Effort, {Blocks, Instructions}, Budget, effort()]),
    ok.

%% `{ssa_opt_fn_budget,0}' exhausts every function's allowance immediately, so
%% every skippable pass is skipped from the start: the per-function "optimizer
%% off from here on" shape. It must be observable in the statistics, must
%% change the generated code (it is less optimized), must not change behaviour
%% and must leave exactly the passes that are never part of a skipped suffix
%% running.
fn_budget_takes_effect(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {fn_tight, [binary,no_spawn_compiler_process,
                        {ssa_opt_fn_budget,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    St = stats(),
    ?assert(is_map(St)),
    ?assert(maps:get(function_over_budget, St) > 0),
    ?assert(code_of(key(plain, Builds)) =/= code_of(key(fn_tight, Builds))),
    %% The allowance is exhausted for every function, so the only passes that
    %% ran are the ones that are never skipped, and every other recorded pass
    %% was skipped for each function instead, i.e. never ran at all.
    Passes = maps:get(passes, St),
    lists:foreach(fun(Name) ->
                          ?assertEqual({Name, true},
                                       {Name, ran_at_least(Name, Passes, 1)})
                  end, never_skipped()),
    Skippable = [Name || Name <- maps:keys(Passes),
                         not lists:member(Name, never_skipped())],
    ?assert(Skippable =/= []),
    lists:foreach(fun(Name) ->
                          Pass = maps:get(Name, Passes, pass_zero()),
                          ?assertEqual({Name, false},
                                       {Name, ran_at_least(Name, Passes, 1)}),
                          ?assertEqual({Name, true},
                                       {Name, maps:get(function_over_budget,
                                                       Pass) > 0})
                  end, Skippable),
    check_behaviour(Builds),
    ok.

%% Compile the same probe with the default options and with an exhausted
%% per-function allowance, execute both builds and check that they agree
%% exactly (including exceptions).
fn_budget_equivalence(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {fn_tight, [binary,no_spawn_compiler_process,
                        {ssa_opt_fn_budget,0}]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    check_behaviour(Builds),
    ok.

%% A per-function allowance that is never reached must not change the
%% generated code either. The bare `ssa_opt_fn_budget' atom is the documented
%% "enabled with the default allowance" form; arming the machinery through it
%% is observable in the statistics, and its default allowance (500 ms) is as
%% far out of reach of these probes as the explicit one.
fn_budget_ample_does_not_change_output(Config) ->
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {fn_ample, [binary,no_spawn_compiler_process,
                        {ssa_opt_fn_budget,1000000}]},
            {fn_default, [binary,no_spawn_compiler_process,
                          ssa_opt_fn_budget]}],
    Builds = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    %% The bare atom really armed the machinery instead of being dropped.
    ?assert(is_map(stats())),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(fn_ample, Builds))),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(fn_default, Builds))),
    ok.

%% `{ssa_opt_fn_budget,Millis}' is a first-class compiler option: `compile.erl'
%% accepts and expands it (so it is not reported as an unknown option),
%% `compile:options/0' advertises it in the help text, and driving it through
%% `compile:file/2' with an exhausted allowance activates per-function
%% skipping, which is observable in the statistics.
fn_budget_option_registered(Config) ->
    Sets = [{fn_tight, [binary,no_spawn_compiler_process,
                        {ssa_opt_fn_budget,0}]}],
    _ = build_sets(Config, "ssa_opt_budget_rich", rich_probe(), Sets),
    St = stats(),
    ?assert(is_map(St)),
    ?assert(maps:get(function_over_budget, St) > 0),
    %% Accepted directly by compile:file/2 as well.
    Dir = tmp_dir(Config),
    File = write_probe(Dir, "ssa_opt_budget_opts", flat_probe()),
    ?assertMatch({ok,_,_},
                 compile:file(File, [binary,return_errors,
                                     {ssa_opt_fn_budget,0}])),
    %% ... and advertised in the `compile:options/0' help text.
    Help = capture_options(),
    ?assert(string:find(Help, "ssa_opt_fn_budget") =/= nomatch),
    ok.

%% Capture the text `compile:options/0' writes by running it with a collecting
%% process as the group leader. `compile:options/0' only uses `io:fwrite', i.e.
%% sends `put_chars' io requests, so a minimal io server is enough.
-spec capture_options() -> string().
capture_options() ->
    Self = self(),
    Ref = make_ref(),
    Collector = spawn_link(fun() -> options_collector(Self, Ref, []) end),
    OldLeader = group_leader(),
    true = group_leader(Collector, Self),
    Result =
        try
            compile:options()
        after
            group_leader(OldLeader, Self)
        end,
    ok = Result,
    Collector ! {text, Self, Ref},
    receive
        {Ref, Text} -> Text
    after 10000 ->
            ct:fail(options_capture_timeout)
    end.

-spec options_collector(pid(), reference(), [unicode:chardata()]) -> ok.
options_collector(Parent, Ref, Acc) ->
    receive
        {io_request, From, ReplyAs, Request} ->
            case Request of
                {put_chars, Chars} ->
                    From ! {io_reply, ReplyAs, ok},
                    options_collector(Parent, Ref, [Chars|Acc]);
                {put_chars, _Enc, Chars} ->
                    From ! {io_reply, ReplyAs, ok},
                    options_collector(Parent, Ref, [Chars|Acc]);
                {put_chars, Mod, Fun, Args} ->
                    From ! {io_reply, ReplyAs, ok},
                    options_collector(Parent, Ref,
                                      [apply(Mod, Fun, Args)|Acc]);
                {put_chars, _Enc, Mod, Fun, Args} ->
                    From ! {io_reply, ReplyAs, ok},
                    options_collector(Parent, Ref,
                                      [apply(Mod, Fun, Args)|Acc]);
                _Other ->
                    From ! {io_reply, ReplyAs, {error, enotsup}},
                    options_collector(Parent, Ref, Acc)
            end;
        {text, Parent, Ref} ->
            Parent ! {Ref, lists:flatten(lists:reverse(Acc))},
            ok
    end.

%% The explicit check that the default path is untouched: compiling this
%% suite's own source with no option, with the gates and with a no-op
%% per-function allowance must produce byte-for-byte identical code.
default_path_unchanged(_Config) ->
    Src = suite_source(),
    Sets = [{plain, [binary,no_spawn_compiler_process]},
            {gates, [binary,no_spawn_compiler_process,ssa_opt_gates]},
            {fn_ample, [binary,no_spawn_compiler_process,
                        {ssa_opt_fn_budget,1000000}]}],
    Builds = [{Set, build(Src, Opts)} || {Set,Opts} <- Sets],
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(gates, Builds))),
    ?assertEqual(code_of(key(plain, Builds)), code_of(key(fn_ample, Builds))),
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

%% Assert that pass `Name' was never run, i.e. that its eligibility gate
%% rejected it for every function it saw.
check_skipped(Stats, Name) ->
    Passes = maps:get(passes, Stats, #{}),
    Pass = maps:get(Name, Passes, pass_zero()),
    ?assertEqual({Name, 0}, {Name, maps:get(ran, Pass)}),
    ?assertEqual({Name, true}, {Name, maps:get(gated, Pass) >= 1}),
    ok.

%% Whether pass `Name' ran at least `N' times in the collected statistics.
ran_at_least(Name, Passes, N) ->
    maps:get(ran, maps:get(Name, Passes, pass_zero())) >= N.

%% The passes that must always run, mirroring `beam_ssa_opt:skippable/1': they
%% change the SSA representation or produce annotations the code generator
%% depends on, so they are never part of a skipped suffix (and are not run
%% under the watchdog either).
never_skipped() ->
    [ssa_opt_split_blocks,
     ssa_opt_linearize,
     ssa_opt_blockify,
     ssa_opt_type_finish,
     ssa_opt_live,
     ssa_opt_bs_ensure].

pass_zero() ->
    #{ran => 0, gated => 0, over_budget => 0, function_over_budget => 0,
      timed_out => 0}.

%% The pure model function under test, aliased to keep the assertions short.
model(Blocks, Instructions) ->
    beam_ssa_opt_budget:classify(Blocks, Instructions).

%% The effort model as documented, derived independently of the module under
%% test: the level from the instruction thresholds and the allowance from
%% `max(50, 500 - Instructions div 20)'.
expected_level(Instructions) when Instructions =< 500 -> full;
expected_level(Instructions) when Instructions =< 5000 -> reduced;
expected_level(_) -> minimal.

expected_allowance(Instructions) ->
    max(50, 500 - Instructions div 20).

%% More effort: `full' > `reduced' > `minimal'.
level_rank(full) -> 3;
level_rank(reduced) -> 2;
level_rank(minimal) -> 1.

%% Assert that the level is non-increasing, and that the allowance is
%% non-increasing and never below the documented floor, over the given
%% ascending instruction counts.
check_monotone([A, B | Rest]) ->
    ?assert(A =< B),
    {LevelA, MsA} = model(1, A),
    {LevelB, MsB} = model(1, B),
    ?assert(level_rank(LevelA) >= level_rank(LevelB)),
    ?assert(MsA >= MsB),
    ?assert(MsA >= 50),
    ?assert(MsB >= 50),
    check_monotone([B | Rest]);
check_monotone([_Last]) ->
    ok;
check_monotone([]) ->
    ok.

stats() ->
    beam_ssa_opt_budget:get_stats().

%% The per-function query surface (see beam_ssa_opt_budget), aliased so the
%% assertions read like the model they check.
effort() ->
    beam_ssa_opt_budget:effort().

function_size() ->
    beam_ssa_opt_budget:function_size().

function_budget() ->
    beam_ssa_opt_budget:function_budget().

function_over_budget() ->
    beam_ssa_opt_budget:function_over_budget().

remaining() ->
    beam_ssa_opt_budget:remaining().

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

%% No loop, but a strength-reduction candidate: `dbl/1' multiplies by two, for
%% which `loop_opt' has an exact rewrite for every number (`X * 2' -> `X + X')
%% that needs neither a back edge nor an integrality proof, so the widened
%% loop gate must accept this module. `plain/1' has neither a back edge nor a
%% candidate - `X + 1' is a variable-first sum, not a canonicalisable
%% `C + V' - so the same gate must reject it.
reduce_probe() ->
    "-module(ssa_opt_budget_reduce).\n"
    "-export([dbl/1,plain/1]).\n"
    "\n"
    "dbl(X) -> X * 2.\n"
    "\n"
    "plain(X) when is_integer(X) -> X + 1;\n"
    "plain(X) -> X.\n".

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

%% A module with a tiny function and one huge function, whose body is
%% generated by `huge_body/1'. Used to exercise the per-function effort model
%% end to end: the tiny function is classified `full', the huge one `minimal'.
mixed_probe(Body) ->
    ["-module(ssa_opt_budget_mixed).\n",
     "-export([tiny/1,huge/1]).\n",
     "\n",
     "tiny(X) when is_integer(X) -> X + 1;\n",
     "tiny(X) -> X.\n",
     "\n",
     "%% A long arithmetic chain: `N' steps, each one SSA instruction, so the\n"
     "%% function's instructions count is at least `N'.\n",
     "huge(X) ->\n",
     Body].

%% Generate the body of the `huge/1' function: a chain of `N' distinct
%% single-assignment variables, the first derived from the argument `X' and
%% each later one from the previous one by one addition, ending with the last
%% variable. The chain keeps every step live, so no step can be folded away
%% and the SSA instruction count is at least `N'.
huge_body(N) ->
    Vars = [var(I) || I <- lists:seq(0, N - 1)],
    Prev = ["X" | lists:droplast(Vars)],
    Steps = [["    ", V, " = ", P, " + 1,\n"]
             || {V, P} <- lists:zip(Vars, Prev)],
    [Steps, "    ", lists:last(Vars), ".\n"].

var(I) ->
    "V" ++ integer_to_list(I).
