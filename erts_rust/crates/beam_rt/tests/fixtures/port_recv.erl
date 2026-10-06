%%% Receiver fixture for `tests/port_recv.rs`.
%%%
%%% It proves the **port → mailbox** delivery path of the scheduler: `recv/0` opens a
%%% spawn-driver port and then blocks in a compiled `receive` on that port's stream, so the
%%% only thing that can wake it is a `{Port, {data, Data}}` message the runtime's poller
%%% delivers into its mailbox (never an explicit `send` from another process).
%%%
%%% Rebuilt with the system `erlc` and committed as `port_recv.beam`; the recipe is in this
%%% directory's `README.md`. Oracle: `port_recv:recv()` ⇒ `<<"hi\n">>`.
-module(port_recv).

-export([recv/0]).

%% Open `{spawn, "echo hi"}` (a shell command line through `/bin/sh -c`) with a `binary`
%% stream and return the first `{Port, {data, Data}}` the port delivers.
recv() ->
    Port = open_port({spawn, "echo hi"}, [binary]),
    receive
        {Port, {data, Data}} -> Data
    end.
