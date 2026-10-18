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
%% The **UDP two-socket parity** fixture for `tests/inet_completion.rs`: the shape that runs
%% under *both* runtimes (this runtime's `run` action and the system `erl`) so their answers
%% can be compared directly.
%%
%% Nothing here blocks: the datagram is sent before the receive is entered, so the answer is
%% a fact about the socket pair, not about a parked process. The printed term is fully
%% **normalised** — the ephemeral ports differ between runs by construction, so the fixture
%% compares them itself and prints booleans:
%%
%%   parity={payload_ok,true}   the datagram arrived, intact, from socket B's own port
-module(cpl_parity).
-export([go/0]).

go() ->
    {ok, A} = gen_udp:open(0, [binary, {active, false}]),
    {ok, PA} = inet:port(A),
    {ok, B} = gen_udp:open(0, [binary, {active, false}]),
    {ok, PB} = inet:port(B),
    ok = gen_udp:send(B, {127, 0, 0, 1}, PA, <<"hi">>),
    R = gen_udp:recv(A, 0, 5000),
    gen_udp:close(A),
    gen_udp:close(B),
    case R of
        {ok, {{127, 0, 0, 1}, From, <<"hi">>}} when is_integer(From) ->
            io:format("parity=~p~n", [{payload_ok, From =:= PB}]);
        Other ->
            io:format("parity=~p~n", [Other])
    end,
    ok.
