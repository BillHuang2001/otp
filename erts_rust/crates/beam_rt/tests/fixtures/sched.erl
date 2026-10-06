%%% Sender fixture for `tests/scheduler.rs`.
%%%
%%% It exists because the committed `../beam_engine/tests/fixtures/recv.beam` can only
%%% send the constant `42` (`recv:bomb/1`), so a **cross-process** selective receive had
%%% no sender that could put a *matching* message after a non-matching head.
%%%
%%% Rebuilt with the system `erlc` and committed as `sched.beam`; the recipe is in this
%%% directory's `README.md`.
-module(sched).

-export([send2/2]).

%% Send the non-matching head `42` first, then the message `recv:scan/0` matches,
%% `{bar, N + 7}`.
%%
%% `N` is a parameter so the tuple is built on this process's heap at **run time**
%% rather than folded into a literal — the ordinary `copy_struct` path a message takes
%% between two processes.
send2(Pid, N) ->
    Pid ! 42,
    Pid ! {bar, N + 7},
    ok.
