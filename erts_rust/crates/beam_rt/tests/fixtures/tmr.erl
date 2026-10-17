%% A **fired runtime timer** fixture for `beam_rt`'s runner tests.
%%
%% Rebuilt with the system `erlc` and committed as `tmr.beam` (the same
%% deterministic scratch-dir recipe as the sibling fixtures — see README.md):
%%
%%     erlc +deterministic tmr.erl
%%
%% `timer:apply_after/4` ends in `timer_server`, which arms
%% `erlang:start_timer/3` and, when it fires, runs its MFA as a **spawned
%% process** — `spawn(erlang, send, [Pid, Msg])`. Delivering the message
%% therefore needs two things a plain `receive … after T` did not have: the
%% host-served BIF entry (`erlang:send/2` dispatched as a BIF, not entered as
%% `erlang.beam`'s stub) and a scheduler pass that fires a due deadline. Answers
%% `done`, or `timeout` when the timer's message never arrived.
-module(tmr).

-export([fire/0]).

fire() ->
    timer:apply_after(50, erlang, send, [self(), done]),
    receive
        M -> M
    after 1500 ->
        timeout
    end.
