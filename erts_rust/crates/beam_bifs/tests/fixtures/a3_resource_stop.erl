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
%% The **Erlang driver** half of the A.3 NIF resource-`stop` / select-STOP-verdict probe
%% (`tests/fixtures/a3_resource_stop.c`). Run it under the live system `erl` oracle; every probe
%% prints exactly one line, so stdout is the measurement of what the real ERTS does:
%%
%%   probe_stop_unknown_fd/0    the C's STOP fast track: an fd no `enif_select` ever armed
%%   probe_arm_then_stop/0      arm READ, then STOP on the same fd — CALLED or SCHEDULED?
%%   probe_double_stop/0        arm READ, STOP, STOP — does the second one double-call?
%%   probe_cancel_then_stop/0   arm READ, CANCEL, STOP
%%   probe_stop_sleep_ms/1      arm READ, STOP, then sleep inside the NIF so a poller-thread
%%                              (deferred) stop has time to land before the report
%%   probe_arm_then_stop_fallback/0     the same on an fd the kernel poll refuses (`/dev/null`), which
%%                              is the ONLY way the deferred `STOP_SCHEDULED` arm is reachable on Linux
%%   probe_stop_fallback_wait_ms/1      ... with a bounded wait, so the poller-thread stop's latency is
%%                              measured instead of guessed
%%   probe_read_on_pending_stop/0       a second request on an fd whose stop is still pending
%%                              (`steal_pending_stop_nif`) — an explicit race, both shapes printed
%%   probe_report/0             the statics the `stop` callback has recorded so far
%%   probe_reset/0              zero those statics between probes
%%   probe_stop_nostop_type/0   the STOP on a resource type WITHOUT a `stop` callback (the C only
%%                              ASSERTs, and ASSERT is a no-op in a release build) — run this one
%%                              in its OWN `erl` invocation, it is expected to take the VM down
%%
%% `all/0` wraps each of the non-fatal probes in `try ... catch C:R -> {C,R} end`, so a raise is
%% data rather than a lost run. Conventions: `Code` is `enif_select`'s return value printed as a
%% signed integer (bit 0 = `ERL_NIF_SELECT_STOP_CALLED`, bit 1 = `ERL_NIF_SELECT_STOP_SCHEDULED`,
%% bit 4 = `ERL_NIF_SELECT_READ_CANCELLED`); `Direct` is the stop callback's `is_direct_call`
%% (`-1` = the callback never ran); `Thr` is `enif_thread_type()` inside the callback
%% (0 undefined, 1 normal scheduler, 2 dirty-cpu, 3 dirty-io).
-module(a3_resource_stop).

-on_load(init/0).

-export([all/0, probe_stop_unknown_fd/0, probe_arm_then_stop/0, probe_double_stop/0,
         probe_cancel_then_stop/0, probe_stop_sleep_ms/1, probe_arm_then_stop_fallback/0,
         probe_stop_fallback_wait_ms/1, probe_read_on_pending_stop/0, probe_reset/0,
         probe_report/0, probe_stop_nostop_type/0]).

init() ->
    Dir = filename:dirname(code:which(?MODULE)),
    erlang:load_nif(filename:join(Dir, "a3_resource_stop"), 0).

%% One line per probe, each guarded so an exception is printed instead of aborting the run.
all() ->
    Each = [{stop_unknown_fd, fun probe_stop_unknown_fd/0},
            {arm_then_stop, fun probe_arm_then_stop/0},
            {double_stop, fun probe_double_stop/0},
            {cancel_then_stop, fun probe_cancel_then_stop/0},
            {stop_sleep_300ms, fun() -> probe_stop_sleep_ms(300) end},
            {arm_then_stop_fallback, fun probe_arm_then_stop_fallback/0},
            {fallback_wait_1000ms, fun() -> probe_stop_fallback_wait_ms(1000) end},
            {read_on_pending_stop, fun probe_read_on_pending_stop/0},
            {report_final, fun probe_report/0}],
    lists:foreach(fun({Name, F}) -> p({Name, guarded(F)}) end, Each),
    ok.

guarded(F) ->
    try F() of
        Result -> Result
    catch
        Class:Reason -> {Class, Reason}
    end.

p(Term) ->
    io:format("~p~n", [Term]).

%% Below: the NIF stubs. A call before `load_nif/2` succeeded raises `undef` from here.
probe_stop_unknown_fd() -> erlang:nif_error(nif_not_loaded).
probe_arm_then_stop() -> erlang:nif_error(nif_not_loaded).
probe_double_stop() -> erlang:nif_error(nif_not_loaded).
probe_cancel_then_stop() -> erlang:nif_error(nif_not_loaded).
probe_stop_sleep_ms(_Ms) -> erlang:nif_error(nif_not_loaded).
probe_arm_then_stop_fallback() -> erlang:nif_error(nif_not_loaded).
probe_stop_fallback_wait_ms(_Ms) -> erlang:nif_error(nif_not_loaded).
probe_read_on_pending_stop() -> erlang:nif_error(nif_not_loaded).
probe_reset() -> erlang:nif_error(nif_not_loaded).
probe_report() -> erlang:nif_error(nif_not_loaded).
probe_stop_nostop_type() -> erlang:nif_error(nif_not_loaded).
