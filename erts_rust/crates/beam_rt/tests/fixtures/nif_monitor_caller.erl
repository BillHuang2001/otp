%%
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
%% The **driver** half of the A.3 `enif_monitor_process` / `down` fixture for `beam_rt`'s
%% `tests/nif_monitor.rs`. It runs unchanged under both this runtime (`run` action) and the system
%% `erl` oracle, prints one tuple per function and returns `ok`, so the two runtimes' stdout is
%% comparable line for line.
%%
%% Conventions:
%%   * every phase arms via `nif_monitor:arm/2` and reports the C's return code — `0` = armed,
%%     `1` = the target has no live process (or the pid is undefined), `-1` = the resource type
%%     has no `down` callback;
%%   * a fired monitor is **polled for** (`wait_down/1`), never assumed: `down` is delivered
%%     asynchronously;
%%   * "the target really is dead" is established with a monitor + its `'DOWN'`, which is the
%%     only reliable signal — a `receive after` proves nothing about another process.
-module(nif_monitor_caller).
-export([arm_live/1, arm_then_kill/1, demonitor_after_arm/1,
         demonitor_after_target_death/1, arm_dead/1, arm_undefined/1, liveness/1]).

p(T) -> io:format("~p~n", [T]).

%% A process that stays alive until it is told to stop, so a monitor installed on it is
%% installed on a process that is definitely alive.
victim() -> spawn(fun() -> receive stop -> ok end end).

%% Wait until `P` really is gone: it is told to stop and the monitor's `'DOWN'` is awaited —
%% the only reliable proof of another process's death (a `receive after` proves nothing).
stop_and_await(P) ->
    Ref = erlang:monitor(process, P),
    P ! stop,
    receive {'DOWN', Ref, process, P, _} -> ok
    after 5000 -> timeout
    end.

%% Poll the `down` count: up to 200 rounds of 5 ms (1 s in total).
wait_down(0) -> nif_monitor:downs();
wait_down(N) ->
    case nif_monitor:downs() of
        0 -> receive after 5 -> ok end, wait_down(N - 1);
        Count -> Count
    end.

%% 1. Arm a monitor on a live process and read the state BEFORE anything dies:
%%    the C's `0` and no `down` yet.
arm_live(Path) ->
    ok = nif_monitor:init(Path),
    R = nif_monitor:make_res(),
    P = victim(),
    A = nif_monitor:arm(R, P),
    p({A, nif_monitor:downs()}),
    exit(P, kill),
    ok.

%% 2. Arm, kill the target, and read what the `down` callback observed: exactly one fire, on
%%    the pid that was armed on.
arm_then_kill(Path) ->
    ok = nif_monitor:init(Path),
    R = nif_monitor:make_res(),
    P = victim(),
    A = nif_monitor:arm(R, P),
    exit(P, kill),
    Count = wait_down(200),
    p({A, Count, nif_monitor:down_pid_matches(P)}),
    ok.

%% 3. `enif_demonitor_process` while the monitor is still armed cancels it: the C's `0`, and
%%    the target's later death fires nothing.
demonitor_after_arm(Path) ->
    ok = nif_monitor:init(Path),
    R = nif_monitor:make_res(),
    P = victim(),
    _A = nif_monitor:arm(R, P),
    D = nif_monitor:demonitor(R),
    exit(P, kill),
    p({D, wait_down(200)}),
    ok.

%% 4. `enif_demonitor_process` after the monitor already fired: the C's `1` (the tree entry
%%    was removed by the fire itself), and the `down` count stays exactly 1.
demonitor_after_target_death(Path) ->
    ok = nif_monitor:init(Path),
    R = nif_monitor:make_res(),
    P = victim(),
    _A = nif_monitor:arm(R, P),
    exit(P, kill),
    Count = wait_down(200),
    D = nif_monitor:demonitor(R),
    p({D, Count}),
    ok.

%% 5. Arming on a pid whose process is already gone: the C's `1`, and no `down` is ever
%%    fabricated for it.
arm_dead(Path) ->
    ok = nif_monitor:init(Path),
    R = nif_monitor:make_res(),
    P = victim(),
    ok = stop_and_await(P),
    A = nif_monitor:arm(R, P),
    p({A, nif_monitor:downs()}),
    ok.

%% 6. Arming on an `enif_set_pid_undefined` target: the C's `1` (the pid check precedes
%%    everything else), and no `down`.
arm_undefined(Path) ->
    ok = nif_monitor:init(Path),
    _R = nif_monitor:make_res(),
    A = nif_monitor:arm_undefined(),
    p({A, nif_monitor:downs()}),
    ok.

%% 7. `enif_is_process_alive`, pinned alongside: `1` for a live process, `0` once it is gone.
liveness(Path) ->
    ok = nif_monitor:init(Path),
    P = victim(),
    L1 = nif_monitor:alive(P),
    ok = stop_and_await(P),
    L2 = nif_monitor:alive(P),
    p({L1, L2}),
    ok.
