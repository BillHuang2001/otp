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
%% The **UDP blocking-receive** fixture for `tests/inet_completion.rs`.
%%
%% `recv/0` opens one passive UDP socket, prints the port it was bound to (so the harness
%% can address it) and then blocks in `gen_udp:recv/3`. Nothing else in this process can
%% produce the awaited message, so the only thing that can end the receive is the runtime's
%% port→mailbox delivery of the datagram's `{inet_async, Port, Id, {ok, Data}}` completion;
%% the value is printed and returned, and `run` prints it again as the action's result.
-module(cpl_udp).
-export([recv/0]).

recv() ->
    {ok, A} = gen_udp:open(0, [binary, {active, false}]),
    {ok, PA} = inet:port(A),
    io:format("udp_port=~p~n", [PA]),
    R = gen_udp:recv(A, 0, 5000),
    io:format("udp_recv=~p~n", [R]),
    gen_udp:close(A),
    R.
