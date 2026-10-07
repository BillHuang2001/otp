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
%%     Upper bound on the wall-clock time of a SINGLE pass. The pass is run
%%     in a monitored child process; when it does not finish in time it is
%%     killed and the compilation is aborted with
%%     `error:{ssa_opt_watchdog_timeout,Pass}'. The pass is deliberately not
%%     skipped; see "Which passes may be skipped" below.
%%
%%   ssa_opt_gates
%%     Enable the eligibility gates and the statistics without setting a
%%     budget or a watchdog (useful to check that the gates are sound).
%%
%% == Which passes may be skipped ==
%%
%% Skipping an SSA optimization pass is only safe when the *whole* rest of
%% the optimizer is skipped with it, because later passes and the code
%% generator rely on shapes that earlier passes produce. Skipping an
%% arbitrary subset of the passes therefore breaks compilation: with a
%% standing budget of 0 a bare `get_map_element' reaches
%% `beam_ssa_codegen:cg_instr/4' (no such clause), and when `ssa_opt_bsm'
%% runs but `ssa_opt_bs_ensure' does not a `bs_match' that skips all trips
%% the assertion in `beam_ssa_pre_codegen:bs_restores_is/5'. Both were
%% observed while compiling e.g. `lib/stdlib/src/string.erl',
%% `lib/kernel/src/file.erl' and `lib/compiler/test/beam_ssa_SUITE.erl'.
%%
%% The machinery is therefore restricted to the one skip discipline that is
%% sound:
%%
%%   * The cumulative budget only ever removes a SUFFIX of the skippable
%%     passes. The deadline is monotone (it never un-expires), so a pass is
%%     skipped only if every later skippable pass is skipped as well, which
%%     is the "optimizer off from here on" configuration the whole-phase
%%     `no_ssa_opt' option already provides. `beam_ssa_opt:skippable/1'
%%     lists the passes that are never part of such a suffix because
%%     skipping them leaves the state inconsistent even then.
%%
%%   * The eligibility gates only skip a pass whose own cheap precondition
%%     proves that it could not have changed anything, which is exactly "the
%%     pass did nothing".
%%
%%   * A watchdog timeout does NOT skip the pass. A timeout can hit any pass
%%     in the middle of the sequence, which would leave an arbitrary, and
%%     (because the outcome of the race between the pass and the timer is
%%     not deterministic) unreproducible subset of the passes unrun; that is
%%     precisely the unsafe configuration described above. A timed-out pass
%%     therefore aborts the compilation with a reported error.
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
                    %% The pass did not finish in time. Do NOT silently skip
                    %% it: skipping a pass in the middle of the pass sequence
                    %% leaves a subset of the optimizations unrun, which
                    %% breaks the code generator and `beam_ssa_pre_codegen'
                    %% (see the module comment). Report the offending pass
                    %% instead.
                    bump(timed_out, Name),
                    erlang:error({ssa_opt_watchdog_timeout, Name});
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
