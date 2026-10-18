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
%% The **native-tier slice-budget** witness (`src/runner/tests/slice_charging.rs`).
%%
%% `heap/1` runs `reds_boundary`'s BIF-and-call-only loop -- each iteration
%% allocates only through registered BIFs (`list_to_tuple(lists:seq(1, 400))`), so
%% `erlc -S` emits no `test_heap`/`allocate_heap` anywhere in this module: the loop
%% body is `allocate`/`call_ext`/`call_ext`/`gc_bif`/`call_last` only -- and then
%% reports the process's own `heap_size`, read *inside* the process after the loop.
%%
%% The self-report is what makes the shape's residual observable at all. A run that
%% never ends a slice is alive only while its single slice executes, so a
%% boundary-sampling harness -- which reads a **live** process's collector between
%% slices -- can never see it: the process leaves the table the moment it returns,
%% and this shape returns inside its first slice. `erlang:process_info(self(),
%% heap_size)` answers the running collector's chain capacity (the young space plus
%% every space a growth retired), which is exactly the figure that climbs when
%% nothing is collected.
%%
%% Every iteration abandons its 400-element tuple, so the live set is a handful of
%% words: a real collection keeps `heap_size` flat, and only never collecting lets
%% it track `N`.
-module(native_reds).

-export([heap/1]).

heap(N) ->
    loop(N),
    heap_size().

loop(0) -> done;
loop(N) -> _ = list_to_tuple(lists:seq(1, 400)), loop(N - 1).

heap_size() ->
    element(2, process_info(self(), heap_size)).
