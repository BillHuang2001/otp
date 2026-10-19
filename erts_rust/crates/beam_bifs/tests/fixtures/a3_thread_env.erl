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
%% Driver for the contract A.3 fixture `a3_thread_env.c` (the sibling C NIF).
%%
%% It answers one question against the live system OTP (the oracle): can the
%% process-independent "message env" of `enif_alloc_env()` / `enif_free_env()`
%% be created on a thread the NIF itself created, and does the
%% `enif_send(NULL, &pid, msg_env, msg)` idiom deliver from there?
%%
%% Run it (the `.so` and the `.beam` must sit in the same directory):
%%
%%     erlc -o $TMPDIR a3_thread_env.erl
%%     erl -noshell -pa $TMPDIR -eval 'a3_thread_env:run(), halt().'
%%
%% `run/0` calls each probe and then RECEIVES what that probe should have
%% delivered, so a message that never arrives is recorded as `timeout` data
%% instead of hanging the run. Every probe call is wrapped so a raise is
%% reported (never swallowed).
-module(a3_thread_env).
-on_load(init/0).

-export([probe_thread_send/0,
         probe_main_env/0,
         probe_worker_atoms/0,
         probe_null_env/0,
         probe_thread_send_to/1,
         run/0]).

%% Bound on waiting for a message a probe should already have delivered.
-define(RECV_TIMEOUT, 2000).

init() ->
    erlang:load_nif(nif_path(), 0).

%% `erlang:load_nif/2` wants the path WITHOUT the extension; the `.so` sits
%% beside the `.beam` in the `-pa` directory.
nif_path() ->
    filename:join(filename:dirname(code:which(?MODULE)), "a3_thread_env").

%% ---- NIF probes (implemented in a3_thread_env.c) ------------------------
%% probe_thread_send/0    -> {EnvIsNull, SendCode, ThreadType} ++ [ThreadCreated]
%% probe_main_env/0       -> {EnvIsNull, SendCode}
%% probe_worker_atoms/0   -> {EnvIsNull, AtomLenOk, AtomTextOk, SendCode,
%%                           ThreadType}
%% probe_null_env/0       -> {EnvIsNull, SendCode, ThreadType}
%% probe_thread_send_to/1 -> {EnvIsNull, SendCode, ThreadType} ++ [ThreadCreated]
%%
%% `ThreadType` is `enif_thread_type()` as answered ON the probed thread
%% (0 == ERL_NIF_THR_UNDEFINED, i.e. "not a scheduler thread"; 1/2/3 == a
%% normal/dirty-CPU/dirty-IO scheduler thread), and `EnvIsNull` is 1 when
%% `enif_alloc_env()` answered NULL (the send is then reported as -1: it never
%% ran).
probe_thread_send() ->
    erlang:nif_error(nif_not_loaded).
probe_main_env() ->
    erlang:nif_error(nif_not_loaded).
probe_worker_atoms() ->
    erlang:nif_error(nif_not_loaded).
probe_null_env() ->
    erlang:nif_error(nif_not_loaded).
probe_thread_send_to(_Pid) ->
    erlang:nif_error(nif_not_loaded).

%% ---- the transcript -----------------------------------------------------

run() ->
    io:format("otp_release=~p erts_version=~s~n",
              [erlang:system_info(otp_release), erlang:system_info(version)]),
    io:format("thread_send=~p~n", [probe(fun probe_thread_send/0)]),
    %% The join happens inside the NIF call, so the message must ALREADY be
    %% queued here — the length is the "delivered before the call returned"
    %% evidence (1, or 0 if the worker's send never happened).
    io:format("thread_send_queue=~p~n", [queue_len()]),
    io:format("~p~n", [recv(thread_msg)]),
    io:format("worker_atoms=~p~n", [probe(fun probe_worker_atoms/0)]),
    io:format("~p~n", [recv(worker_atom_msg)]),
    io:format("main_env=~p~n", [probe(fun probe_main_env/0)]),
    io:format("~p~n", [recv(call_path_msg)]),
    io:format("null_env=~p~n", [probe(fun probe_null_env/0)]),
    io:format("~p~n", [recv(null_env_msg)]),
    P = park_receiver(),
    io:format("parked_status=~p~n", [wait_parked(P, ?RECV_TIMEOUT)]),
    io:format("thread_send_to=~p~n",
              [probe(fun() -> probe_thread_send_to(P) end)]),
    io:format("~p~n", [recv(parked_msg)]),
    ok.

queue_len() ->
    element(2, process_info(self(), message_queue_len)).

%% A process that parks in `receive` and reports what it got: the parked
%% receiver of probe_thread_send_to/1.
park_receiver() ->
    Parent = self(),
    spawn(fun() ->
                  receive
                      Msg -> Parent ! {parked_got, Msg}
                  end
          end).

%% Poll until the receiver really is parked (`waiting`), so the "wake a parked
%% receiver" leg is a verified precondition rather than a sleep-and-hope race.
wait_parked(Pid, Left) when Left =< 0 ->
    {timeout, Pid, process_info(Pid, status)};
wait_parked(Pid, Left) ->
    case process_info(Pid, status) of
        {status, waiting} ->
            {status, waiting};
        _Other ->
            timer:sleep(1),
            wait_parked(Pid, Left - 1)
    end.

probe(Fun) ->
    try Fun()
    catch Class:Reason -> {Class, Reason}
    end.

recv(Label) ->
    receive
        Msg -> {Label, Msg}
    after ?RECV_TIMEOUT ->
            {Label, timeout}
    end.
