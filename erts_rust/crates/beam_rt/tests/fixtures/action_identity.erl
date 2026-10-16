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
%% The **action-identity** fixture (`tests/action_identity.rs`).
%%
%% An `-s`/`-run` action discards its result, so the only way to observe *which
%% process* it runs in is to have the action itself print what it sees. The
%% oracle runs such an action in a process `init` spawned -- an ordinary child
%% that is **not** registered under any name -- so `process_info(self(),
%% registered_name)` is `[]`, `whereis(init) =:= self()` is `false`, `self()` is
%% a live pid and `node(self()) =:= node()` holds. `ident/0` reports all four, on
%% one line, in a fixed order.
%%
%% `tests/action_identity.rs` compiles this at test time with the system `erlc`
%% into a scratch directory and passes that directory as the only `-pa` of both
%% runtimes; the fixture is never committed as a `.beam`.
-module(action_identity).
-export([ident/0]).

ident() ->
    io:format("rn=~p eq=~p ispid=~p nodeok=~p~n",
              [erlang:process_info(self(), registered_name),
               erlang:whereis(init) =:= self(),
               is_pid(self()),
               node(self()) =:= node()]),
    ok.
