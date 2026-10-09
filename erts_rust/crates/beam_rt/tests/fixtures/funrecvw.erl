%% The cross-module half of the fun-bodied-process-entry gate
%% (`tests/fun_entry_receive.rs`, fixture `funrecv.erl`).
%%
%% The spawning/fun-creating code lives in `funrecv`; this module owns the `receive` that
%% blocks **one `call_ext` away** from the fun body — exactly how `proc_lib:sync_start/2`
%% (and `application_controller:init/1` behind it) sits relative to
%% `proc_lib:start_link/4`.
-module(funrecvw).

-export([wait_ready/1, wait_go/0]).

%% The waiting `receive` of the `run_cross/0` shape: announce first (so the parent knows
%% the child reached its `receive`), then block until the parent's `{go, Parent}`, then ack
%% it with the value both runtimes must print.
wait_ready(Parent) ->
    Parent ! {ready, self()},
    receive
        {go, Parent} -> Parent ! {ack, self(), 42}
    end.

%% The waiting `receive` of the API-level test's child (`funrecv:mkchild/0`): no handshake
%% at all, so the caller can observe the process parked before anything wakes it.
wait_go() ->
    receive
        {go, V} -> V
    end.
