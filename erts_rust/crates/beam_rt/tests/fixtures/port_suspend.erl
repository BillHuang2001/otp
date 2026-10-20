%% The **busy-port park → pump → resume** fixture for `beam_rt`'s in-process
%% BIF-suspend probe (`src/port/busy_suspend_tests.rs`, contract A.4).
%%
%% Both exports issue ONE plain `port_command/2` — no options, so the C's
%% `ERTS_PORT_OP_BUSY` arm applies (`io.c:1763-1769`) — against a port the caller
%% is handed, and the harness makes that port **busy** before it starts: the first
%% attempt therefore finds a busy port and the caller must be suspended and the
%% instruction re-executed (the C's `erts_suspend` + `ERTS_BIF_YIELD3` re-run,
%% `erl_bif_port.c:205-231`).
%%
%% `command/1` is the shape a **harness-driven** clear needs: the process parks on
%% its first slice, the harness clears the busy bit (what a driver's `$N`
%% `port_control` does), and the next slice re-executes the BIF against an idle
%% port. The harness can therefore assert the park itself — nothing written, no
%% result yet — before the clear.
%%
%% `command_after/2` is the same call with the clear **armed as a deadline** by a
%% helper process: the helper blocks on `receive ... after Delay` (a runtime-native
%% timer deadline — no `timer_server` is involved, so this works on the in-process
%% scheduler route too), then runs `erlang:port_control(Port, $N, <<>>)`, which is
%% the C driver's clear. One `Runtime::run_scheduled` call therefore drives the
%% whole round trip: the caller parks, the pass pumps, the timer half fires the
%% helper's deadline, the clear lands, and the re-issued command writes. Because the
%% helper parks on its very first slice, the caller's first attempt always meets the
%% busy port — no wall-clock figure is asserted anywhere.
-module(port_suspend).
-export([command/1, command_after/2]).

command(Port) ->
    port_command(Port, <<"x">>).

command_after(Port, Delay) ->
    _ = spawn(fun() ->
                      receive after Delay -> ok end,
                      port_control(Port, $N, <<>>)
              end),
    port_command(Port, <<"x">>).
