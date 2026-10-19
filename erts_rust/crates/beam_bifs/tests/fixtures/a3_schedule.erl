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
%% The **Erlang driver** half of the `enif_schedule_nif` success-path probe for contract A.3:
%% one stub per entry of `tests/fixtures/a3_schedule.c`, loaded through `-on_load` /
%% `erlang:load_nif/2` from a `.so` sitting NEXT TO this module's `.beam` (both are expected in the
%% same directory, as the measurement recipe does: compile the `.so` into `$TMPDIR`, copy this file
%% there, `erlc -o $TMPDIR`, then run `erl -pa $TMPDIR`).
%%
%% Each probe is a separate exported function so the caller can wrap it in `try ... catch C:R:S`
%% and record a raise as data. What each one pins (see the C file's header for the `erl_nif.c`
%% line numbers):
%%
%%   sched_sum(2, 3)          -> {scheduled,5}   the continuation's own value is the call's result
%%   sched_loop(100000)       -> {done,Yields}   a constant-stack self-rescheduling chain
%%   sched_raise()            -> raises `boom`   a raising continuation stops the chain
%%   sched_bad_flags()        -> raises `badarg` flags = 0x1000 is refused (`erl_nif.c:3597`)
%%   sched_too_many_args()    -> raises `badarg` argc = 300 > MAX_ARG (`erl_nif.c:3569`)
%%   sched_dirty(N)           -> {dirty,N}       an ERL_NIF_DIRTY_JOB_CPU_BOUND schedule
%%   sched_last_ret()         -> 0               the RAW word enif_schedule_nif returned
%%   sched_thread_types(0|1)  -> {threads,CallerType,ContType,Flags}
%%   sched_ghost_name(N)      -> {ghost,N}       a `fun_name` that exists nowhere still works
%%
%% The `cont_*` functions are exported as stubs because `erlang:load_nif/2` validates EVERY row of
%% the C `ErlNifFunc` table against this module's own function table (`erl_nif.c:4874-4878`): a row
%% naming a function the module does not have fails the whole load with
%% `{error,{bad_lib,"Function not found M:F/A"}}`. They are entered by POINTER from the trampoline,
%% never by the Erlang caller.
%%
%% A one-liner per probe, with the raise captured as `{Class,Reason,Stacktrace}`:
%%
%%   Pr = fun(L,F) -> io:format("~s: ~p~n", [L, (try F() catch C:R:S -> {C,R,S} end)]) end,
%%   Pr("A", fun() -> a3_schedule:sched_sum(2,3) end),
%%   Pr("A_ret", fun() -> a3_schedule:sched_last_ret() end),
%%   ...
-module(a3_schedule).
-on_load(init/0).
-export([init/0, sched_sum/2, sched_loop/1, sched_raise/0, sched_bad_flags/0,
         sched_too_many_args/0, sched_dirty/1, sched_last_ret/0, sched_thread_types/1,
         sched_ghost_name/1, sched_reargv/1,
         cont_sum/2, cont_loop/1, cont_raise/0, cont_dirty/1, cont_ttype/1, cont_ghost/1,
         cont_reargv/1]).

%% `-on_load` runs before the module is callable, so the `.so` is loaded for every probe and a
%% failed load makes the whole module unloadable instead of every call raising `undef`.
init() ->
    So = filename:join(filename:dirname(code:which(?MODULE)), "a3_schedule"),
    erlang:load_nif(So, 0).

sched_sum(_N, _M) -> erlang:nif_error(nif_not_loaded).
sched_loop(_N) -> erlang:nif_error(nif_not_loaded).
sched_raise() -> erlang:nif_error(nif_not_loaded).
sched_bad_flags() -> erlang:nif_error(nif_not_loaded).
sched_too_many_args() -> erlang:nif_error(nif_not_loaded).
sched_dirty(_N) -> erlang:nif_error(nif_not_loaded).
sched_last_ret() -> erlang:nif_error(nif_not_loaded).
sched_thread_types(_Dirty) -> erlang:nif_error(nif_not_loaded).
sched_ghost_name(_N) -> erlang:nif_error(nif_not_loaded).
sched_reargv(_N) -> erlang:nif_error(nif_not_loaded).

%% Continuations: reached only from the C trampoline (kept for `load_nif`'s table check).
cont_sum(_N, _M) -> erlang:nif_error(nif_not_loaded).
cont_loop(_N) -> erlang:nif_error(nif_not_loaded).
cont_raise() -> erlang:nif_error(nif_not_loaded).
cont_dirty(_N) -> erlang:nif_error(nif_not_loaded).
cont_ttype(_Flags) -> erlang:nif_error(nif_not_loaded).
cont_ghost(_N) -> erlang:nif_error(nif_not_loaded).
cont_reargv(_N) -> erlang:nif_error(nif_not_loaded).
