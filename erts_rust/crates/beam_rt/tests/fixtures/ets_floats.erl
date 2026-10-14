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
%% The float-literal-carrying ETS insert/lookup fixture (`ets_floats.beam`), driven by
%% `tests/rss_bound.rs`. It mirrors the `copy_bound` benchmark workload's `ets_copy` case
%% (`../../tools/bench/src/workloads/copy_bound.rs`, `module_source/0`) with the module and
%% function renamed: every iteration builds the workload's structured payload -- a map whose
%% `b` value is the tuple `{ok, <<"hello world">>, 3.5}` and therefore carries a FLOAT
%% LITERAL (whose `fmove {float,K} {fr,N}` constant load is what makes the whole module
%% float-family) -- inserts it into a one-key ETS table, reads it back, and checks the value
%% with a bound-variable pattern so a corrupted copy raises. Each validated round-trip adds
%% 1, so `run(N) == N`.
%%
%% The table is created and deleted once per `run/1`, so it never grows: the per-iteration
%% garbage is the payload, the inserted tuple and the lookup result -- exactly the retention
%% the collector has to reclaim for the peak RSS to stay flat.
-module(ets_floats).
-export([run/1]).

%% The one structured payload every iteration copies: a map holding a 10-element list, a
%% tuple (an atom + a binary + a float), a 12-byte binary, a char-list string and a negative
%% integer.
payload() ->
    #{a => [1,2,3,4,5,6,7,8,9,10],
      b => {ok, <<"hello world">>, 3.5},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42}.

run(N) ->
    Tab = ets:new(ets_floats, [set, public]),
    Result = run(N, Tab, 0),
    ets:delete(Tab),
    Result.

run(0, _Tab, Acc) -> Acc;
run(N, Tab, Acc) ->
    T = payload(),
    true = ets:insert(Tab, {k, T}),
    case ets:lookup(Tab, k) of
        [{k, T}] -> run(N - 1, Tab, Acc + 1)
    end.
