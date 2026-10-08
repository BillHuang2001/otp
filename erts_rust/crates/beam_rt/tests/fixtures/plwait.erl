%% The cross-module half of the `proc_lib:start/3,4` bisection (contract A.9, the
%% self-hosted boot).
%%
%% `tests/fixtures/plstart.erl` owns every "spawn a child, wait for its ack" shape;
%% this module owns the one shape that is *split across a module boundary*: the
%% spawning code lives in `plstart`, the waiting `receive` here. That boundary is
%% exactly what `proc_lib:start/4` crosses (`application_controller:start/1` →
%% `proc_lib:start_link/4` → `sync_start/2`), so it is the discriminator the
%% integration suite pins.
%%
%% Compiled at test time by `tests/proc_lib_start.rs`; both modules are compiled into
%% the same scratch directory, which is handed to this runtime and to the system `erl`
%% as a single `-pa`.
-module(plwait).

-export([wait/2, wait_hard/2, plain/1]).

%% The waiting `receive` of the exact `proc_lib:start/4` shape: a bare receive whose
%% pattern binds the child pid — `proc_lib:sync_start/2` verbatim.
wait(Pid, Ref) ->
    receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    end.

%% As above, plus a *hardcoded* `after` clause: the delay is `Operand::Integer` in the
%% compiled code, so the runtime arms a real deadline for it (see `runner/reason.rs`).
wait_hard(Pid, Ref) ->
    receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    after 2000 -> timeout
    end.

%% The non-blocking control: the cross-module call itself is not the problem, only a
%% cross-module call that *blocks* is.
plain(N) -> N + 1.
