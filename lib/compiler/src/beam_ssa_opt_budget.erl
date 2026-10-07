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
%% Per-SSA-pass time budget, watchdog and eligibility hook.
%%
%% This module provides the framework that bounds how much time the SSA
%% optimization phases of `beam_ssa_opt' may spend, and that lets a pass
%% declare a cheap precondition which the driver checks before running it.
%% Everything here works on OPAQUE state: the state term and the pass fun
%% are taken and returned unchanged, so this module knows nothing about
%% `#opt_st{}', block maps or the function database.
%%
%% == Opt-in, default off ==
%%
%% Nothing in this module changes the behaviour of the compiler unless one
%% of the new compile options is given, and when no option is given
%% `wrap_fun/4' returns the original pass fun unchanged, so the default
%% compilation path is `Fun(State)' by construction:
%%
%%   {ssa_opt_budget, Millis} / ssa_opt_budget
%%     Cumulative time budget for the SSA optimization phases. Once the
%%     budget is exhausted the watchdog driver stops running skippable
%%     passes and `beam_ssa_opt' stops the fixpoint iteration. This is
%%     equivalent to lowering `?MAX_REPETITIONS' to zero and to disabling
%%     every skippable optimization pass: less optimized, equally correct.
%%
%%   {ssa_opt_watchdog, Millis} / ssa_opt_watchdog
%%     Upper bound on the wall-clock time of a SINGLE skippable pass. The
%%     pass is run in a monitored child process; when it does not finish in
%%     time it is killed and treated as if it had not run at all.
%%
%%   ssa_opt_gates
%%     Enable the eligibility gates and the statistics without setting a
%%     budget or a watchdog (useful to check that the gates are sound).
%%
%% == Equivalence ==
%%
%% Every pass to which the machinery is applied is a pure `State -> State'
%% function with no observable side effects. Skipping one, either because
%% its eligibility gate says that it cannot fire or because it ran out of
%% time, is exactly "the pass did nothing", which the pass framework
%% already supports as a first-class configuration: `beam_ssa_opt:passes_1/3'
%% derives a `no_<name>' option for every pass and substitutes the identity
%% function for a disabled one. The result is therefore less optimized but
%% equally correct. Passes that change the representation of the SSA
%% (map <-> linear) or its block granularity are never skipped by the
%% driver; see `beam_ssa_opt:skippable/1'.
%%
%% == Process dictionary ==
%%
%% The per-compilation state lives in the process dictionary of the process
%% that runs `beam_ssa_opt:module/2' (the compiler process, unless
%% `no_spawn_compiler_process' is given, see `compile:file/2'). It is
%% initialized by `init/1' and observed with `get_stats/0'. A watchdog child
%% starts with a copy of the parent's process dictionary, so a pass that
%% reads process-dictionary state (for example the FBIP flag `$fbip_opt' set
%% by `beam_ssa_opt:late_epilogue_passes/1') sees exactly what it would have
%% seen when run in the parent.

-module(beam_ssa_opt_budget).
-moduledoc false.

-export([init/1, enabled/0, wrap_fun/4, run/5, over_budget/0, get_stats/0]).

-export_type([stats/0, gate/0]).

%% Default cumulative budget and per-pass watchdog, in milliseconds, used
%% when the option is given without an explicit value.
-define(DEFAULT_BUDGET_MS, 5000).
-define(DEFAULT_WATCHDOG_MS, 1000).

%% Process-dictionary key holding the per-compilation state.
-define(KEY, '$ssa_opt_budget').

-type state() :: term().

%% A cheap precondition over the pass state. `undefined' means that the pass
%% declares no precondition and is always run.
-type gate() :: undefined | fun((state()) -> boolean()).

-type pass_stats() :: #{ran := non_neg_integer(),
                        gated := non_neg_integer(),
                        over_budget := non_neg_integer(),
                        timed_out := non_neg_integer()}.

-type stats() :: #{ran := non_neg_integer(),
                   gated := non_neg_integer(),
                   over_budget := non_neg_integer(),
                   timed_out := non_neg_integer(),
                   passes := #{atom() => pass_stats()}}.

-define(ZERO_PASS_STATS, #{ran => 0, gated => 0,
                           over_budget => 0, timed_out => 0}).

%%============================================================================
%% Setup
%%============================================================================

%% Initialize the budget state for one compilation. Must be called before
%% the phase pass lists are built, because the wrapping of the pass funs
%% happens while they are built. Clears the state when no option is given.
-spec init([compile:option()]) -> ok.
init(Opts) ->
    Budget = opt_ms(ssa_opt_budget, ?DEFAULT_BUDGET_MS, Opts),
    Watchdog = opt_ms(ssa_opt_watchdog, ?DEFAULT_WATCHDOG_MS, Opts),
    Gates = lists:member(ssa_opt_gates, Opts),
    case {Budget, Watchdog, Gates} of
        {infinity, infinity, false} ->
            erase(?KEY),
            ok;
        _ ->
            Deadline = case Budget of
                           infinity -> infinity;
                           Ms -> erlang:monotonic_time(millisecond) + Ms
                       end,
            put(?KEY, #{deadline => Deadline,
                        watchdog => Watchdog,
                        stats => new_stats()}),
            ok
    end.

%% Read a millisecond option. `Key' means "enabled with the default value",
%% `{Key,Millis}' sets an explicit value, and an absent option means
%% `infinity' (disabled).
opt_ms(Key, Default, Opts) ->
    case proplists:get_value(Key, Opts, undefined) of
        undefined -> infinity;
        true -> Default;
        Ms when is_integer(Ms), Ms >= 0 -> Ms;
        _Other -> Default
    end.

%% True when the budget machinery is active for the current compilation.
-spec enabled() -> boolean().
enabled() ->
    is_map(get(?KEY)).

%% True once the cumulative budget has been exhausted.
-spec over_budget() -> boolean().
over_budget() ->
    case get(?KEY) of
        #{deadline := infinity} ->
            false;
        #{deadline := Deadline} ->
            erlang:monotonic_time(millisecond) >= Deadline;
        _ ->
            false
    end.

%% Return the statistics of the most recent compilation that had the
%% machinery enabled, or `undefined'. Only observable in the process that
%% ran the compilation (see the module comment).
-spec get_stats() -> stats() | undefined.
get_stats() ->
    case get(?KEY) of
        #{stats := Stats} -> Stats;
        _ -> undefined
    end.

%%============================================================================
%% Wrapping a pass
%%============================================================================

%% Wrap a pass fun so that the eligibility gate, the cumulative budget and
%% the per-pass watchdog are honoured. When the machinery is disabled the
%% original fun is returned, so the default path is untouched.
%%
%% `Skippable' is false for passes that must always run.
-spec wrap_fun(atom(), fun((state()) -> state()), gate(), boolean()) ->
          fun((state()) -> state()).
wrap_fun(Name, Fun, Gate, Skippable) ->
    case enabled() of
        false ->
            Fun;
        true ->
            fun(State) -> run(Name, Fun, Gate, Skippable, State) end
    end.

%% Run one pass under the eligibility gate, the cumulative budget and the
%% watchdog. Also used directly by passes that are invoked from another pass
%% rather than from a phase pass list (the opt-in FBIP pass).
-spec run(atom(), fun((state()) -> state()), gate(), boolean(), state()) ->
          state().
run(Name, Fun, Gate, Skippable, State) ->
    case get(?KEY) of
        undefined ->
            Fun(State);
        _ ->
            case gate_ok(Gate, Name, State) of
                true ->
                    case Skippable andalso over_budget() of
                        true ->
                            bump(over_budget, Name),
                            State;
                        false ->
                            timed_run(Name, Fun, Skippable, State)
                    end;
                false ->
                    %% The pass cannot fire for this state; skipping it is
                    %% equivalent to running it.
                    State
            end
    end.

gate_ok(undefined, _Name, _State) ->
    true;
gate_ok(Gate, Name, State) ->
    case Gate(State) of
        true ->
            true;
        false ->
            bump(gated, Name),
            false
    end.

%%============================================================================
%% Watchdog
%%============================================================================

timed_run(Name, Fun, Skippable, State) ->
    case watchdog() of
        infinity ->
            ran(Name, Fun, State);
        _Ms when not Skippable ->
            ran(Name, Fun, State);
        Ms ->
            case run_child(Fun, State, Ms) of
                {ok, Result} ->
                    bump(ran, Name),
                    Result;
                timeout ->
                    %% The pass did not finish in time. Treat it as if it had
                    %% not run: the input state is a valid SSA state, so the
                    %% result is less optimized but equally correct.
                    bump(timed_out, Name),
                    State;
                {raise, Class, Reason, Stack} ->
                    erlang:raise(Class, Reason, Stack)
            end
    end.

ran(Name, Fun, State) ->
    Result = Fun(State),
    bump(ran, Name),
    Result.

watchdog() ->
    case get(?KEY) of
        #{watchdog := Ms} -> Ms;
        _ -> infinity
    end.

%% Run the pass in a monitored child process and give up after `Ms'
%% milliseconds. The child starts with a copy of the parent's process
%% dictionary so that the pass sees the same process-dictionary state.
run_child(Fun, State, Ms) ->
    Parent = self(),
    Ref = make_ref(),
    Env = erlang:get(),
    {Pid, Mon} = spawn_monitor(fun() -> child(Parent, Ref, Env, Fun, State) end),
    receive
        {Ref, Result} ->
            erlang:demonitor(Mon, [flush]),
            Result;
        {'DOWN', Mon, process, Pid, Reason} ->
            %% The child always sends its result before exiting, so this can
            %% only happen if it was killed from the outside. Do not swallow
            %% it.
            erlang:error({ssa_opt_watchdog_child_died, Reason})
    after Ms ->
        erlang:exit_signal(Pid, kill),
        receive
            {'DOWN', Mon, process, Pid, _} -> ok
        after 0 ->
            ok
        end,
        timeout
    end.

child(Parent, Ref, Env, Fun, State) ->
    lists:foreach(fun({Key, Value}) -> put(Key, Value) end, Env),
    Result = try Fun(State) of
                 Value -> {ok, Value}
             catch
                 Class:Reason:Stack -> {raise, Class, Reason, Stack}
             end,
    Parent ! {Ref, Result}.

%%============================================================================
%% Statistics
%%============================================================================

new_stats() ->
    #{ran => 0, gated => 0, over_budget => 0, timed_out => 0, passes => #{}}.

bump(Key, Name) ->
    case get(?KEY) of
        undefined ->
            ok;
        State ->
            put(?KEY, bump_1(Key, Name, State))
    end.

bump_1(Key, Name, State) ->
    Stats0 = maps:get(stats, State),
    Passes0 = maps:get(passes, Stats0),
    Pass0 = maps:get(Name, Passes0, ?ZERO_PASS_STATS),
    Passes = Passes0#{Name => Pass0#{Key => maps:get(Key, Pass0) + 1}},
    Stats = Stats0#{Key => maps:get(Key, Stats0) + 1, passes => Passes},
    State#{stats := Stats}.
