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
%% The allocate-and-abandon churn fixture (`churn.beam`), driven by
%% `tests/rss_bound.rs`. It is the `gc_bound` benchmark workload's embedded module
%% (`../../tools/bench/src/workloads/gc_bound.rs`) with the module/function renamed:
%% one churn builds three 256-sized structures -- a list (interpreted recursion), a
%% binary and a tuple -- abandons them for the collector, and checks the two
%% O(1)-checkable ones so a silently-dropped allocation fails loudly. Each churn adds
%% 1, so `run(N) == N`.
-module(churn).
-export([run/1]).

run(N) -> run(N, 0).

run(0, Acc) -> Acc;
run(N, Acc) ->
    _ = build_list(256, []),
    Bin = list_to_binary(build_bytes(256, [])),
    Tup = list_to_tuple(build_list(256, [])),
    true = (byte_size(Bin) =:= 256),
    true = (tuple_size(Tup) =:= 256),
    run(N - 1, Acc + 1).

build_list(0, Acc) -> Acc;
build_list(N, Acc) -> build_list(N - 1, [N | Acc]).

build_bytes(0, Acc) -> Acc;
build_bytes(N, Acc) -> build_bytes(N - 1, [N rem 256 | Acc]).
