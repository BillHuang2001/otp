%%
%% The parked-death reclamation fixture (`beam_rt/tests/parked_exit_reclaim.rs`).
%%
%% `hold/2` builds `N` `Bytes`-byte binaries through compiled `bs_create_bin` code — `Bytes` is a
%% RUNTIME value, so each one is a genuine OFF-HEAP refc payload above the 64-byte on-heap limit —
%% and keeps them ALL live in a list, then parks in an untimed `receive`. The process's machine
%% reservoir therefore holds `N` payloads (they are reachable from the parked frame, so every
%% slice-boundary prune retains them) for as long as the process sits in that receive.
%%
%% That is the state the suite needs: a live parked process whose reservoir is many MiB, which
%% `kill/1` (untrappable `exit(Pid, kill)`) or `stop/1` (`Pid ! stop`) then ends.
-module(pk).
-export([hold/2, kill/1, stop/1]).

%% Build the payloads, keep every one of them live, then park for ever (no `after`).
hold(Bytes, N) ->
    Held = build(Bytes, N, []),
    receive
        stop -> {stopped, length(Held)};
        _Other -> {unexpected, length(Held)}
    end.

build(_Bytes, 0, Acc) ->
    Acc;
build(Bytes, N, Acc) ->
    build(Bytes, N - 1, [<<0:(Bytes*8)>> | Acc]).

%% The untrappable kill: a plain `exit/2` with the `kill` reason.
kill(Pid) ->
    exit(Pid, kill).

%% The ordinary wake: a message the parked `receive` matches, so the process resumes, returns
%% and dies through the scheduler's normal `RunOutcome::Exit(normal)`.
stop(Pid) ->
    Pid ! stop.
