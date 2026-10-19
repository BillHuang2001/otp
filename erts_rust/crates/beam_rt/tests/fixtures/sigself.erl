% %CopyrightBegin%
%
% SPDX-License-Identifier: Apache-2.0
%
% Copyright Ericsson AB 2025-2026. All Rights Reserved.
%
% Licensed under the Apache License, Version 2.0 (the "License");
% you may not use this file except in compliance with the License.
% You may obtain a copy of the License at
%
%     http://www.apache.org/licenses/LICENSE-2.0
%
% Unless required by applicable law or agreed to in writing, software
% distributed under the License is distributed on an "AS IS" BASIS,
% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
% See the License for the specific language governing permissions and
% limitations under the License.
%
% %CopyrightEnd%
% A monitor/link signal a process generates for ITSELF must be visible to the
% `receive` that follows it in the SAME reduction slice.
%
% Every shape below does the signalling and the `receive` in one process with no
% intervening blocking call, so the receiver is still `Running` when the signal is
% queued. The oracle materialises the signal into the mailbox inside the receive
% scan (`i_loop_rec` -> `erts_proc_sig_receive_helper` -> `erts_proc_sig_fetch`),
% so no shape ever times out.
-module(sigself).
-export([killed/0, natural/0, killed_after/0, ran_before_kill/0,
         spawn_monitor_kill/0, trapping_link/0, bare_receive/0, not_matching/0,
         all/0]).

%% A killed monitor target: the DOWN must match with no timeout.
killed() ->
    P = spawn(fun() -> receive stop -> ok end end),
    MRef = monitor(process, P),
    exit(P, kill),
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% A natural death: the child is told to stop, so the DOWN carries `normal`.
natural() ->
    P = spawn(fun() -> receive stop -> ok end end),
    MRef = monitor(process, P),
    P ! stop,
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% The same as `killed/0` but with a pre-armed timeout in the matching receive,
%% so a missed signal shows up as the `after` branch rather than a block.
killed_after() ->
    P = spawn(fun() -> receive stop -> ok end end),
    MRef = monitor(process, P),
    exit(P, kill),
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 1000 -> timeout
    end.

%% The child has certainly run to its first `receive` before it is killed, so the
%% kill takes the "live, blocked target" path rather than the "not yet scheduled" one.
ran_before_kill() ->
    Parent = self(),
    P = spawn(fun() -> Parent ! ready, receive stop -> ok end end),
    MRef = monitor(process, P),
    receive ready -> ok after 2000 -> no_ready end,
    exit(P, kill),
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% `spawn_monitor` instead of `spawn` + `monitor/2`: the monitor is armed at spawn.
spawn_monitor_kill() ->
    {P, MRef} = spawn_monitor(fun() -> receive stop -> ok end end),
    exit(P, kill),
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% A trapping linked process: the signal becomes a message (`{'EXIT', P, killed}`)
%% instead of killing the receiver.
trapping_link() ->
    process_flag(trap_exit, true),
    P = spawn_link(fun() -> receive stop -> ok end end),
    exit(P, kill),
    receive
        {'EXIT', P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% A bare `receive` with no timeout at all: a missed signal blocks forever.
bare_receive() ->
    P = spawn(fun() -> receive stop -> ok end end),
    MRef = monitor(process, P),
    exit(P, kill),
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    end.

%% The materialised DOWN must respect the receive pattern: a scan that skips the
%% queued DOWN and then matches a later message must not lose the DOWN.
not_matching() ->
    P = spawn(fun() -> receive stop -> ok end end),
    MRef = monitor(process, P),
    exit(P, kill),
    receive
        {something, other} -> wrong
    after 0 -> none
    end,
    receive
        {'DOWN', MRef, process, P, Reason} -> Reason
    after 2000 -> timeout
    end.

%% Every shape in one call, in the order the suite prints them.
all() ->
    [{killed, killed()},
     {natural, natural()},
     {killed_after, killed_after()},
     {ran_before_kill, ran_before_kill()},
     {spawn_monitor_kill, spawn_monitor_kill()},
     {trapping_link, trapping_link()},
     {bare_receive, bare_receive()},
     {not_matching, not_matching()}].
