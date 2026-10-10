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
%% Per-SSA-pass time budget, watchdog, eligibility hook and per-function
%% effort model.
%%
%% This module is the SINGLE decision point for how much time and effort the
%% SSA optimization phases of `beam_ssa_opt' may spend, at three levels: the
%% whole compilation (a cumulative budget), a single pass (a watchdog) and a
%% single function (an effort level plus a wall-clock allowance).
%% It is also the hook through which a pass declares a cheap precondition.
%% Everything here works on OPAQUE state except for reading the function id
%% and the SSA size out of it; the state term and the pass fun are otherwise
%% taken and returned unchanged.
%%
%% == Opt-in, default off ==
%%
%% Nothing in this module changes the behaviour of the compiler unless one
%% of the new compile options is given, and when no option is given
%% `init/1' erases the state and `wrap_fun/4' returns the original pass fun
%% unchanged, so the default compilation path is `Fun(State)' by
%% construction and its output is byte-for-byte identical:
%%
%%   {ssa_opt_budget, Millis} / ssa_opt_budget
%%     Cumulative time budget for the SSA optimization phases. Once the
%%     budget is exhausted the driver stops running skippable passes and
%%     `beam_ssa_opt' stops the fixpoint iteration. This is equivalent to
%%     lowering `?MAX_REPETITIONS' to zero and to disabling every skippable
%%     optimization pass: less optimized, equally correct.
%%     Also enables per-function skipping (see below).
%%
%%   {ssa_opt_fn_budget, Millis} / ssa_opt_fn_budget
%%     Per-function allowance, overriding the size-derived one for every
%%     function. Enables per-function skipping. `Millis' = 0 exhausts every
%%     function's allowance immediately, so every skippable pass is skipped
%%     from the start, which is the "optimizer off from here on" shape.
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
%%     budget, a per-function allowance or a watchdog (useful to check that
%%     the gates are sound).
%%
%% == Per-function effort model ==
%%
%% The effort level of a function is derived from its SSA size alone, where
%% the size is the number of instructions (the `#b_blk.is' entries of all
%% blocks of the function, see `function_size/0'): `full' for at most
%% ?FN_FULL_MAX (500) instructions, `reduced' for at most ?FN_REDUCED_MAX
%% (5000) instructions and `minimal' above that.
%% The rationale is that the optimization passes are super-linear in the
%% size of the function they are handed, so the point of diminishing returns
%% is reached much earlier for a huge function than for a small one.
%% A pass that consults `effort/0' can therefore drop its most expensive
%% transformation for a `reduced' or `minimal' function while still doing
%% the cheap and the local ones, which is what bounds a huge function.
%% The number of blocks is reported to `classify/2' but does NOT influence
%% the thresholds or the allowance, which are computed from the number of
%% instructions alone.
%% Only FUNCTION-level runs are classified: a module-level pass is handed no
%% function, so it is registered nowhere and gets no effort level.
%%
%% == Per-function allowance ==
%%
%% Each function also gets a wall-clock allowance of
%% `max(?FN_MIN_MS, ?FN_BASE_MS - Size div ?FN_INSTRS_PER_MS)' milliseconds,
%% i.e. of 50 ms at the least and of 500, 475, ... 50 ms as the size grows
%% (?FN_BASE_MS = 500, ?FN_MIN_MS = 50, ?FN_INSTRS_PER_MS = 20).
%% A tiny function therefore gets the full 500 ms and a 9000-instruction
%% function or larger hits the 50 ms floor.
%% The allowance thus shrinks with the size of the function and is floored,
%% so a huge function cannot blow up compile time: the bigger the function,
%% the less time it gets.
%% The allowance is fixed the first time the framework sees a function, when
%% the deadline `now + allowance' is recorded in the process dictionary
%% together with the size and the level; the deadline never moves forward,
%% so the set of passes a function still *gets* may shrink but can never
%% grow, for the life of the compilation, across phases and fixpoint
%% repetitions.
%% `{ssa_opt_fn_budget,Millis}' replaces the size-derived allowance with
%% `Millis' for every function; it accepts only a non-negative integer and
%% any other value falls back to the size-derived allowance.
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
%%   * The per-function allowance removes a SUFFIX of the skippable passes
%%     of that *function*, for the same reason: its deadline is monotone as
%%     well, so a function that runs out of its allowance simply stops being
%%     further optimized, exactly as if `no_ssa_opt' had been given for it.
%%     Functions are optimized independently of each other (each one owns
%%     its own state in the state map and no pass requires two functions to
%%     be equally optimized), so one function's suffix going unrun cannot
%%     make another function's state inconsistent. The per-function bound is
%%     only consulted when per-function skipping is enabled, i.e. when a
%%     cumulative budget or an explicit per-function allowance was given;
%%     with `ssa_opt_gates' alone the output is unchanged, and the per-pass
%%     `no_<pass>' kill switches remain the way to turn single passes off.
%%     A pass that is not marked skippable is NEVER skipped, and a skipped
%%     pass is always skipped together with the rest of the suffix; no
%%     middle pass of a function is ever dropped on its own.
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
%% initialized by `init/1', updated by `run/5' and observed with
%% `get_stats/0' and the `function_size/0' / `function_budget/0' /
%% `function_over_budget/0' / `effort/0' / `remaining/0' queries. A watchdog
%% child starts with a copy of the parent's process dictionary, so a pass
%% that reads process-dictionary state sees exactly what it would have seen
%% when run in the parent.
%%
%% == Query surface for the opt-in passes ==
%%
%% The passes this framework exists for are the opt-in `beam_ssa_loop_opt'
%% (`loop_opt'), `beam_ssa_fus' (`fusion_opt') and `beam_ssa_fbip'
%% (`fbip_opt'); they should use the framework like this:
%%
%%   * Call `effort/0' once at entry. It is `undefined' when the machinery
%%     is off, which means "do everything as before"; `none' means that the
%%     global budget is exhausted and the pass should do nothing at all;
%%     `full' means "do everything"; for `reduced' or `minimal' the pass
%%     should skip its expensive transformation (for example a whole-
%%     function analysis or a multi-key rewrite) and keep only the cheap,
%%     local rewrites. `function_over_budget/0' tells whether it is the
%%     current function's own allowance (rather than the compilation's
%%     budget) that is exhausted.
%%   * Use `over_budget/0' / `remaining/0' to bail out early from a pass
%%     that is already running on a huge function instead of running an
%%     O(n^2) analysis to completion.
%%   * Use `function_size/0' and `function_budget/0' when a pass wants to
%%     report or budget against the size of the function it is handed
%%     instead of using the fixed effort levels.
%%   * `classify/2' is pure and exported so that the model can be exercised
%%     directly; passes do not need it.
%%   * A MODULE-level pass (one that is handed `{StMap,FuncDb}' instead of a
%%     single function's state, such as the opt-in FBIP pass
%%     `beam_ssa_fbip:opt/2') goes through `run/5' as well, and is therefore
%%     gated, budgeted and watched over in the same way. It is not a
%%     function, though, so it is registered in no per-function bookkeeping:
%%     the per-function queries are `undefined' while it runs and its own
%%     allowance is never consulted (only the cumulative budget and the
%%     watchdog bound it, in addition to its gate).
%%
%% == Statistics ==
%%
%% `get_stats/0' reports the counters of the most recent compilation:
%% `ran', `gated', `over_budget' (skipped because the cumulative budget was
%% exhausted), `function_over_budget' (skipped because the current
%% function's own allowance was exhausted), `timed_out', `effort' (how many
%% distinct functions were first classified at each level; a module-level
%% pass contributes no function) and `passes' (the same counters per pass).
%% The counters are only collected when the machinery is enabled.
-module(beam_ssa_opt_budget).
-moduledoc false.

-export([init/1, enabled/0, wrap_fun/4, run/5, over_budget/0, get_stats/0,
         remaining/0, function_size/0, function_budget/0,
         function_over_budget/0, effort/0, classify/2]).

-export_type([stats/0, gate/0, effort/0]).

-include("beam_ssa.hrl").
-import_record(beam_ssa_opt, [opt_st]).

%% Default cumulative budget and per-pass watchdog, in milliseconds, used
%% when the option is given without an explicit value.
-define(DEFAULT_BUDGET_MS, 5000).
-define(DEFAULT_WATCHDOG_MS, 1000).

%% Size thresholds of the per-function effort model, in instructions.
-define(FN_FULL_MAX, 500).
-define(FN_REDUCED_MAX, 5000).

%% Per-function time allowance: `max(?FN_MIN_MS,
%% ?FN_BASE_MS - Size div ?FN_INSTRS_PER_MS)' milliseconds.
-define(FN_BASE_MS, 500).
-define(FN_MIN_MS, 50).
-define(FN_INSTRS_PER_MS, 20).

%% Process-dictionary key holding the per-compilation state.
-define(KEY, '$ssa_opt_budget').

-type state() :: term().

%% A cheap precondition over the pass state. `undefined' means that the pass
%% declares no precondition and is always run.
-type gate() :: undefined | fun((state()) -> boolean()).

%% The effort a function is allowed. `none' means "do nothing at all".
-type effort() :: full | reduced | minimal | none.

%% The SSA size of a function: number of blocks and total number of
%% instructions.
-type size() :: {Blocks :: non_neg_integer(),
                 Instructions :: non_neg_integer()}.

-type pass_stats() :: #{ran := non_neg_integer(),
                        gated := non_neg_integer(),
                        over_budget := non_neg_integer(),
                        function_over_budget := non_neg_integer(),
                        timed_out := non_neg_integer()}.

-type stats() :: #{ran := non_neg_integer(),
                   gated := non_neg_integer(),
                   over_budget := non_neg_integer(),
                   function_over_budget := non_neg_integer(),
                   timed_out := non_neg_integer(),
                   effort := #{full := non_neg_integer(),
                               reduced := non_neg_integer(),
                               minimal := non_neg_integer()},
                   passes := #{atom() => pass_stats()}}.

-define(ZERO_PASS_STATS, #{ran => 0, gated => 0, over_budget => 0,
                           function_over_budget => 0, timed_out => 0}).

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
    FnBudget = opt_ms(ssa_opt_fn_budget, ?FN_BASE_MS, Opts),
    Gates = lists:member(ssa_opt_gates, Opts),
    case {Budget, Watchdog, FnBudget, Gates} of
        {infinity, infinity, infinity, false} ->
            erase(?KEY),
            ok;
        _ ->
            Deadline = case Budget of
                           infinity -> infinity;
                           Ms -> now_ms() + Ms
                       end,
            %% Per-function skipping is only enabled by an explicit bound:
            %% either the cumulative budget or the per-function allowance.
            %% With the gates alone (or the watchdog alone) the per-function
            %% deadlines are recorded and reported, but never acted upon.
            FnSkip = Budget =/= infinity orelse FnBudget =/= infinity,
            put(?KEY, #{deadline => Deadline,
                        watchdog => Watchdog,
                        fn_budget => FnBudget,
                        fn_skip => FnSkip,
                        fns => #{},
                        current => undefined,
                        stats => new_stats()}),
            ok
    end.

%% Read a millisecond option. `Key' means "enabled with the default value",
%% `{Key,Millis}' sets an explicit value, and an absent option means
%% `infinity' (disabled). An unusable value falls back to the default.
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
    case global_deadline() of
        infinity -> false;
        Deadline -> now_ms() >= Deadline
    end.

%% Milliseconds left in the cumulative budget, or `infinity' when there is
%% no cumulative budget.
-spec remaining() -> infinity | non_neg_integer().
remaining() ->
    case global_deadline() of
        infinity -> infinity;
        Deadline -> max(0, Deadline - now_ms())
    end.

global_deadline() ->
    case get(?KEY) of
        #{deadline := Deadline} -> Deadline;
        _ -> infinity
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
%% Per-function effort model
%%============================================================================

%% Classify a function by its SSA size and return the effort level it is
%% allowed together with the time allowance (in milliseconds) that level
%% implies. The number of blocks is part of the size description, but both
%% the thresholds and the allowance are computed from the number of
%% instructions alone (see the module comment). Pure: the function does not
%% read any state, so it can be exercised directly.
-spec classify(Blocks :: non_neg_integer(), Instructions :: non_neg_integer()) ->
          {full | reduced | minimal, non_neg_integer()}.
classify(_Blocks, Instructions) when is_integer(Instructions) ->
    Size = max(0, Instructions),
    Level = if
                Size =< ?FN_FULL_MAX -> full;
                Size =< ?FN_REDUCED_MAX -> reduced;
                true -> minimal
            end,
    {Level, max(?FN_MIN_MS, ?FN_BASE_MS - Size div ?FN_INSTRS_PER_MS)}.

%% The size of the function the framework most recently saw, or `undefined'
%% when the machinery is disabled, no function has been seen yet, or the most
%% recent run was a module-level pass.
-spec function_size() -> size() | undefined.
function_size() ->
    case get(?KEY) of
        #{current := {_Id, Size}} -> Size;
        _ -> undefined
    end.

%% The time allowance (milliseconds) of the current function, or
%% `undefined' when the machinery is disabled, no function has been seen, or
%% the most recent run was a module-level pass.
-spec function_budget() -> non_neg_integer() | undefined.
function_budget() ->
    case fn_entry() of
        #{allowance := Ms} -> Ms;
        _ -> undefined
    end.

%% True when the current function's own allowance is exhausted. Never true
%% while the machinery is disabled, before the first function is seen, or
%% during a module-level pass (which has no allowance of its own).
-spec function_over_budget() -> boolean().
function_over_budget() ->
    case fn_entry() of
        #{deadline := Deadline} -> now_ms() >= Deadline;
        _ -> false
    end.

%% The effort allowed for the current function. `undefined' when the
%% machinery is disabled, no function has been seen yet, or the most recent
%% run was a module-level pass; `none' when the cumulative budget is
%% exhausted (nothing is allowed any more), otherwise the level the current
%% function was classified at.
-spec effort() -> effort() | undefined.
effort() ->
    case get(?KEY) of
        #{} ->
            case over_budget() of
                true -> none;
                false ->
                    case fn_entry() of
                        #{level := Level} -> Level;
                        _ -> undefined
                    end
            end;
        _ ->
            undefined
    end.

%% The per-function bookkeeping of the current function, or `undefined' when
%% the machinery is disabled, no function has been seen, or the most recent
%% run was a module-level pass (`observe/1' records the atom `module' as the
%% current run in that case, which matches neither clause below).
fn_entry() ->
    case get(?KEY) of
        #{current := {Id, _Size}, fns := Fns} -> maps:get(Id, Fns, undefined);
        _ -> undefined
    end.

fn_skipping_active() ->
    case get(?KEY) of
        #{fn_skip := Skip} -> Skip;
        _ -> false
    end.

%% Take note of the function the pass fun was handed: extract its id and its
%% SSA size, classify it when it is seen for the first time, and remember
%% the size as "the size of the most recently seen function". A module-level
%% pass (a state that does not describe a single function) has no function
%% to classify: nothing is registered, and the state is marked as such so
%% that the per-function allowance is not applied to it.
observe(State) ->
    case get(?KEY) of
        undefined ->
            ok;
        BudgetState ->
            case fn_info(State) of
                {Id, Size} ->
                    put(?KEY, observe_1(Id, Size, BudgetState));
                module ->
                    put(?KEY, BudgetState#{current := module})
            end
    end.

observe_1(Id, Size, #{fns := Fns} = State) ->
    case is_map_key(Id, Fns) of
        true ->
            State#{current := {Id, Size}};
        false ->
            Fn = new_fn(Size, State),
            Stats = bump_effort(Fn, maps:get(stats, State)),
            State#{fns := Fns#{Id => Fn},
                   current := {Id, Size},
                   stats := Stats}
    end.

%% The per-function entry recorded the first time a function is seen. The
%% deadline is fixed here and never revised, which is what makes the skipped
%% set a monotone suffix of the function's passes.
new_fn(Size, State) ->
    {Blocks, Instructions} = Size,
    {Level, Default} = classify(Blocks, Instructions),
    Allowance = case maps:get(fn_budget, State) of
                    infinity -> Default;
                    Ms -> Ms
                end,
    #{deadline => now_ms() + Allowance,
      allowance => Allowance,
      size => Size,
      level => Level}.

bump_effort(#{level := Level}, Stats) ->
    Effort0 = maps:get(effort, Stats),
    Effort = Effort0#{Level => maps:get(Level, Effort0) + 1},
    Stats#{effort := Effort}.

%% The function id and the SSA size carried by the pass state, or `module'
%% for a state that does not describe a single function (a module-level
%% pass, such as the opt-in FBIP pass). Only the function-level state
%% `{#opt_st{},FuncDb}' carries a function's id and size.
fn_info(State) ->
    try
        {#opt_st{ssa=Ssa, anno=Anno}, _FuncDb} = State,
        %% `Anno' is the function's anno map; `func_info' is the entry
        %% `beam_ssa_opt:get_func_id/1' reads. (`beam_ssa:get_anno/2' takes
        %% a construct, not a bare anno map.)
        {maps:get(func_info, Anno, undefined), ssa_size(Ssa)}
    catch
        _:_ -> module
    end.

%% The number of blocks and the total number of instructions of a
%% function's SSA, which is either a linear block list or a block map.
ssa_size(Ssa) when is_map(Ssa) ->
    maps:fold(fun(_Label, Blk, Acc) -> size_add(Blk, Acc) end, {0, 0}, Ssa);
ssa_size(Ssa) when is_list(Ssa) ->
    lists:foldl(fun({_Label, Blk}, Acc) -> size_add(Blk, Acc) end, {0, 0}, Ssa);
ssa_size(_Ssa) ->
    {0, 0}.

size_add(Blk, {Blocks, Instructions}) ->
    {Blocks + 1, Instructions + blk_size(Blk)}.

blk_size(#b_blk{is=Is}) when is_list(Is) -> length(Is);
blk_size(_Blk) -> 0.

%%============================================================================
%% Wrapping a pass
%%============================================================================

%% Wrap a pass fun so that the eligibility gate, the cumulative budget, the
%% per-function allowance and the per-pass watchdog are honoured. When the
%% machinery is disabled the original fun is returned, so the default path
%% is untouched.
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

%% Run one pass under the eligibility gate, the cumulative budget, the
%% per-function allowance and the watchdog. Also used directly for the opt-in
%% FBIP pass, which is a module-level pass and is therefore not wrapped by
%% `wrap_fun/4' (see `beam_ssa_opt:epilogue_module_passes/1').
-spec run(atom(), fun((state()) -> state()), gate(), boolean(), state()) ->
          state().
run(Name, Fun, Gate, Skippable, State) ->
    case get(?KEY) of
        undefined ->
            Fun(State);
        _ ->
            observe(State),
            case gate_ok(Gate, Name, State) of
                true ->
                    case skip_reason(Skippable) of
                        none ->
                            timed_run(Name, Fun, Skippable, State);
                        global ->
                            bump(over_budget, Name),
                            State;
                        function ->
                            bump(function_over_budget, Name),
                            State
                    end;
                false ->
                    %% The pass cannot fire for this state; skipping it is
                    %% equivalent to running it.
                    State
            end
    end.

%% Why a pass is skipped, if it may be skipped at all. A pass that is not
%% skippable is never skipped, whatever the bounds say (see the module
%% comment). The cumulative budget is checked first, so a skip caused by the
%% compilation's budget is counted as such even when the function's own
%% allowance happens to be exhausted as well. The per-function allowance
%% only applies to a function-level run: a module-level pass (which
%% `observe/1' marks as `module', see `fn_entry/0') is bounded by the
%% cumulative budget alone.
skip_reason(false) ->
    none;
skip_reason(true) ->
    case over_budget() of
        true ->
            global;
        false ->
            case fn_skipping_active() andalso function_over_budget() of
                true -> function;
                false -> none
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
    #{ran => 0, gated => 0, over_budget => 0, function_over_budget => 0,
      timed_out => 0,
      effort => #{full => 0, reduced => 0, minimal => 0},
      passes => #{}}.

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

%%============================================================================
%% Time
%%============================================================================

now_ms() ->
    erlang:monotonic_time(millisecond).
