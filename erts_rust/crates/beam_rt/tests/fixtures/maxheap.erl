%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2025-2026. All Rights Reserved.
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
%% The `max_heap_size` **enforcement** fixture (`maxheap.beam`), driven by
%% `tests/max_heap_flag.rs` through the scheduler `run` route (the only route that can
%% observe enforcement: the `-eval` process is never enqueued, so a verdict latched on it
%% is never drained).
%%
%% Every function here is written so that the two runtimes can be compared **byte for
%% byte**: no pid and no reference is ever printed (`is_pid/1` and `is_reference/1`
%% replace them), and no runtime-specific figure is part of a return value. The only
%% runtime-specific observables are the `=ERROR REPORT====` blocks on stderr, which the
%% suite pins separately.
%%
%% * `kill_at_bound/0` — a **trapping linked parent** observes a victim that set
%%   `process_flag(trap_exit, true)` and then over-ran `#{size => 1000}` die with the bare
%%   atom `killed`. The victim's `try … catch` proves the kill is not an exception.
%% * `kill_at_bound_monitor/0` — the same victim, watched by a monitor: `'DOWN' … killed`.
%% * `log_and_continue/0` — `kill => false, error_logger => true`: the victim survives and
%%   runs to completion.
%% * `log_and_continue_silent/0` — the same workload with `error_logger => false`: not one
%%   report.
%% * `reports_per_collection/0` — the measurement behind "one report per over-limit
%%   collection": the victim reports the **runtime's own** collection-counter delta
%%   around its work, so the suite can compare it with the number of reports.
%% * `capacity_not_occupancy/0` — a victim that never holds more than ~60 words of live
%%   data still breaches `#{size => 700}`: the check is on the collected **capacity**.
%% * `floor/0` — the `size` floor matrix (`224`…`235` around the default
%%   `min_heap_size` 233, plus `0`, a negative, a float, a map without `size`).
%% * `spawn_opt_*/0` — `spawn_opt`'s `{max_heap_size, Map}` really carries the flags.
-module(maxheap).

-export([
    kill_at_bound/0,
    kill_at_bound_monitor/0,
    log_and_continue/0,
    log_and_continue_silent/0,
    reports_per_collection/0,
    capacity_not_occupancy/0,
    floor/0,
    spawn_opt_log/0,
    spawn_opt_kill/0,
    spawn_opt_info/0,
    spawn_opt_floor/0
]).

%% ------------------------------------------------------------------------------------
%% 1 — the kill at the bound
%% ------------------------------------------------------------------------------------

kill_at_bound() ->
    process_flag(trap_exit, true),
    Parent = self(),
    Child = spawn_link(fun() -> victim(Parent) end),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'EXIT', Child, Reason} ->
            {died, is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

kill_at_bound_monitor() ->
    Parent = self(),
    Child = spawn(fun() -> victim(Parent) end),
    Ref = erlang:monitor(process, Child),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'DOWN', Ref, process, Child, Reason} ->
            {down, is_reference(Ref), is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

%% The victim traps exits and wraps its work in a `try … catch`: a kill must be neither
%% trappable nor catchable, so the only way the parent hears from it is the `'EXIT'`/
%% `'DOWN'` with the bare atom `killed`.
victim(Parent) ->
    process_flag(trap_exit, true),
    _ = process_flag(max_heap_size, #{size => 1000}),
    Done =
        try grow(20000, []) of
            _ -> completed
        catch
            _:_ -> caught
        end,
    Parent ! {done, self(), Done},
    ok.

%% ------------------------------------------------------------------------------------
%% 2 — log and continue
%% ------------------------------------------------------------------------------------

log_and_continue() ->
    process_flag(trap_exit, true),
    Parent = self(),
    Child = spawn_link(fun() -> logger(Parent, true) end),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'EXIT', Child, Reason} ->
            {died, is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

log_and_continue_silent() ->
    process_flag(trap_exit, true),
    Parent = self(),
    Child = spawn_link(fun() -> logger(Parent, false) end),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'EXIT', Child, Reason} ->
            {died, is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

logger(Parent, ErrorLogger) ->
    _ = process_flag(max_heap_size, #{
        size => 1000, kill => false, error_logger => ErrorLogger
    }),
    _ = grow(2000, []),
    Parent ! {done, self(), completed},
    ok.

%% ------------------------------------------------------------------------------------
%% 2b — the "one report per over-limit collection" measurement
%% ------------------------------------------------------------------------------------

%% The victim reports the **runtime's own** node-global collection counter delta around
%% its over-limit work, so the suite can hold it against the number of reports the runtime
%% emitted. Both figures are this runtime's own state; neither is compared with the
%% oracle.
reports_per_collection() ->
    process_flag(trap_exit, true),
    Parent = self(),
    Child = spawn_link(fun() -> counted(Parent) end),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'EXIT', Child, Reason} ->
            {died, is_pid(Child), Reason}
    after 20000 ->
        timeout
    end.

counted(Parent) ->
    %% The bound sits **below the very first capacity a collection installs**, so *every*
    %% collection in the measured window is over the limit and the report count is exactly
    %% the collection count. (With a larger bound the first collection is still within it,
    %% and the two figures differ by one — measured, which is why this is pinned with 300.)
    _ = process_flag(max_heap_size, #{size => 300, kill => false, error_logger => true}),
    Before = element(1, erlang:statistics(garbage_collection)),
    _ = grow(400, []),
    After = element(1, erlang:statistics(garbage_collection)),
    Parent ! {done, self(), After - Before},
    ok.

%% ------------------------------------------------------------------------------------
%% 4 — capacity, not occupancy
%% ------------------------------------------------------------------------------------

%% The victim holds ten two-tuples — under a hundred words — for its whole life, and is
%% still over `#{size => 700}`: the bound is checked against the heap **capacity a
%% collection installs**, never against the live (or occupied) words.
capacity_not_occupancy() ->
    process_flag(trap_exit, true),
    Parent = self(),
    Child = spawn_link(fun() -> small(Parent) end),
    receive
        {done, Child, Info} ->
            {survived, Info};
        {'EXIT', Child, Reason} ->
            {died, is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

small(Parent) ->
    _ = process_flag(max_heap_size, #{size => 700, kill => false, error_logger => true}),
    Live = grow(100, []),
    Parent ! {done, self(), {length(Live), is_list(Live)}},
    ok.

%% ------------------------------------------------------------------------------------
%% 3 — the `badarg` floor
%% ------------------------------------------------------------------------------------

%% Every candidate runs on a caller that immediately resets the bound (`size => 0`), and
%% the accepted shapes carry `kill => false, error_logger => false` so a stray collection
%% cannot turn a parse probe into a kill or a report.
floor() ->
    [
        try_set(#{size => 232, kill => false, error_logger => false}),
        try_set(#{size => 233, kill => false, error_logger => false}),
        try_set(#{size => 234, kill => false, error_logger => false}),
        try_set(#{size => 0}),
        try_set(0),
        try_set(-1),
        try_set(1.5),
        try_set(#{kill => true})
    ].

try_set(Value) ->
    Answer =
        try
            _ = process_flag(max_heap_size, Value),
            accepted
        catch
            error:Reason -> Reason
        end,
    _ = process_flag(max_heap_size, #{size => 0}),
    Answer.

%% ------------------------------------------------------------------------------------
%% 5 — `spawn_opt`'s `{max_heap_size, Map}` carries the flags
%% ------------------------------------------------------------------------------------

spawn_opt_log() ->
    Parent = self(),
    Child = spawn_opt(
        fun() -> opt_logger(Parent) end,
        [{max_heap_size, #{size => 1000, kill => false, error_logger => true}}]
    ),
    Ref = erlang:monitor(process, Child),
    receive
        {done, Child, Info} ->
            {survived, is_reference(Ref), Info};
        {'DOWN', Ref, process, Child, Reason} ->
            {died, is_reference(Ref), Reason}
    after 15000 ->
        timeout
    end.

opt_logger(Parent) ->
    _ = grow(2000, []),
    Parent ! {done, self(), completed},
    receive
        stop -> ok
    after 5000 -> ok
    end.

spawn_opt_kill() ->
    Parent = self(),
    Child = spawn_opt(
        fun() -> opt_victim(Parent) end,
        [{max_heap_size, #{size => 1000}}]
    ),
    Ref = erlang:monitor(process, Child),
    receive
        {done, Child, Info} ->
            {survived, is_reference(Ref), Info};
        {'DOWN', Ref, process, Child, Reason} ->
            {died, is_reference(Ref), is_pid(Child), Reason}
    after 15000 ->
        timeout
    end.

opt_victim(Parent) ->
    Done =
        try grow(20000, []) of
            _ -> completed
        catch
            _:_ -> caught
        end,
    Parent ! {done, self(), Done},
    receive
        stop -> ok
    after 5000 -> ok
    end.

%% What `process_info(P, max_heap_size)` reports for a child born with `spawn_opt`'s
%% setting: the whole flag map, rendered in the C's own key order.
spawn_opt_info() ->
    Child = spawn_opt(
        fun() -> park() end,
        [{max_heap_size, #{size => 2000, kill => false, error_logger => false}}]
    ),
    Info = element(2, process_info(Child, max_heap_size)),
    Child ! stop,
    [{Key, maps:get(Key, Info)} || Key <- [error_logger, include_shared_binaries, kill, size]]
    ++ [maps:size(Info)].

park() ->
    receive
        stop -> ok
    after 5000 -> ok
    end.%% The `spawn_opt` floor: `{min_heap_size, 1000}` rounds up through the growth table, so
%% the child's own floor is above 1000 and a `max_heap_size` below it is `badarg` while the
%% rounded figure is accepted.
spawn_opt_floor() ->
    [
        spawn_floor(#{size => 1000}),
        spawn_floor(#{size => 1597}),
        spawn_floor(#{size => 1598})
    ].

spawn_floor(Setting) ->
    try
        _ = spawn_opt(
            fun() -> park() end,
            [{min_heap_size, 1000}, {max_heap_size, Setting}]
        ),
        accepted
    catch
        error:Reason -> Reason
    end.

%% ------------------------------------------------------------------------------------
%% The workload: `grow/2` builds a live list of `N` two-tuples, so the heap must actually
%% grow (and therefore collect) as `N` rises.
%% ------------------------------------------------------------------------------------

grow(0, Acc) -> Acc;
grow(N, Acc) -> grow(N - 1, [{N, N + 1} | Acc]).
