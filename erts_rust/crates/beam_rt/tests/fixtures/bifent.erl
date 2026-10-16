%% A BIF-export **spawn** fixture for `beam_rt`'s runner tests.
%%
%% Rebuilt with the system `erlc` and committed as `bifent.beam` (the same
%% deterministic scratch-dir recipe as the sibling fixtures — see README.md):
%%
%%     erlc +deterministic bifent.erl
%%
%% Both exports pin the *first-entry* dispatch of a process whose entry names an
%% MFA the host serves rather than code: `spawn(erlang, send, [Pid, Msg])` must
%% reach `erlang:send/2` as a BIF, never enter `erlang.beam`'s own `send/2` stub
%% (`func_info; move undefined -> x0; call_ext_only erlang:nif_error/1`, which
%% raises the bare atom `undefined`). `timer_server`'s `do_apply/2` runs exactly
%% that spawn shape for a `timer:apply_after/4` MFA, so this fixture is the
%% minimal reproducer of the fired-timer message that was never delivered.
-module(bifent).

-export([fire/0, queued/0]).

%% `spawn(erlang, send, [self(), done])` — the spawned BIF process must deliver
%% `done` into this process's mailbox. Answers `done`, or `timeout` when nothing
%% arrived within the window (the pre-fix behaviour).
fire() ->
    spawn(erlang, send, [self(), done]),
    receive
        M -> M
    after 1500 ->
        timeout
    end.

%% The regression guard: a message already in the mailbox when the receive
%% starts is still matched — the timer wait must not delay or drop it.
queued() ->
    self() ! done,
    receive
        M -> M
    after 1500 ->
        timeout
    end.
