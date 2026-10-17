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
%% The **reduction/yield-boundary** collection fixture (`reds_boundary.beam`), driven by
%% `src/runner/tests/reds_door.rs`.
%%
%% The loop body allocates solely through registered BIFs (`lists:seq/2` and the
%% `list_to_tuple/1` result copy-out), so the compiler emits **no** `test_heap`/
%% `allocate_heap` at all: `erlc -S` on this file yields no such instruction anywhere in
%% `loop/2` (only `call_ext`/`call`/`is_eq_exact`/`move`/`bif` shapes). Nothing in this
%% module therefore reaches the engine's heap-admission door, which is exactly the shape
%% whose nursery used to grow with N instead of being collected: `alloc_words` and the
%% BIF-result copy-out (`RuntimeContext::move_result_into_process_heap`) can only grow the
%% nursery.
%%
%% Each iteration abandons its 400-element tuple, so the live set is a handful of words and
%% a real collection keeps the heap flat; `run(N) == N`.
-module(reds_boundary).
-export([run/1]).
run(N) -> loop(N), N.
loop(0) -> done;
loop(N) -> _ = list_to_tuple(lists:seq(1, 400)), loop(N - 1).
