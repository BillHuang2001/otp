%% Fixture for `tests/port_monitor_cleanup.rs`: the **watcher** half of the dead-watcher port
%% monitor sweep (contract A.4).
%%
%% `erlang:monitor(port, Port)` records an *incoming* monitor on the port (so a port close can
%% deliver `{'DOWN', Ref, port, Port, Reason}`) **and** an *outgoing* entry on the watching
%% process. When the watcher dies first, `beam_process`'s finalizer cannot remove the incoming
%% entry — this runtime hands no port table to the termination path — so `beam_rt`'s exit
%% observer records the dying watcher's pairs and a scheduler-pass drain sweeps them.
%%
%% The two shapes the test needs:
%%
%% * `watch/1` — record the monitor and then **park**, so the incoming entry is observable
%%   *before* the death (this is what makes the test discriminate: without a live watcher the
%%   monitor set could be empty for the wrong reason);
%% * `kill/1`  — untrappably kill a pid, which is the death that must leave the port's monitor
%%   set.
%%
%% Compiled at test time with the system `erlc` (never committed as `.beam`); a missing
%% toolchain is a **loud** skip, never a silent pass.
-module(port_watch).

-export([watch/1, kill/1]).

%% Watch the live port `Port` and park: the process stays alive (so the incoming monitor it
%% just recorded is observable) until something kills it.
watch(Port) ->
    erlang:monitor(port, Port),
    receive
        stop -> ok
    end.

%% Untrappably kill `Pid` — the death whose dangling port monitor must be swept.
kill(Pid) ->
    erlang:exit(Pid, kill).
