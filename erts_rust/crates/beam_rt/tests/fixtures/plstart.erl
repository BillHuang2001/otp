%% The `proc_lib:start/3,4` blocker (contract A.9, the self-hosted boot).
%%
%% `init`'s `kernelProcess application_controller {application_controller,start}`
%% directive brings the application controller up through `proc_lib:start_link/4`, and this
%% module is the minimal bisection of *why* that call does not answer here: each exported
%% function is one shape of "spawn a child, wait for its ack", from the working inline
%% control to the exact `proc_lib` structure.
%%
%% Every entry point returns a **pid-free** shape (see `shape/1`), because the two runtimes
%% hand out different pids but must print the same line.
%%
%% Compiled at test time by `tests/proc_lib_start.rs` (together with `plwait.erl`, into one
%% scratch directory that is the single `-pa` for both runtimes) and run under both
%% runtimes.
-module(plstart).

-export([run/0, startlink/0,
         child/0, child_explicit/0,
         inline/0, inline_hard/0,
         sep/0, sep_hard/0,
         cross/0, cross_hard/0, pure/0,
         child_ack/1, child_ack2/0,
         plshaped/0, plsync/2, plspawn_mon/3, plinit_p/5,
         myname/0]).

%% One answer shape both runtimes can print identically: a `{ok, Pid}` becomes
%% `{ok, pid}` (the pids themselves differ), everything else passes through.
shape({ok, Pid}) when is_pid(Pid) -> {ok, pid};
shape(Other) -> Other.

%% --- the blocker: the real `proc_lib:start/4` --------------------------------

%% Entry point 1: exactly the call `application_controller:start/1` is reached through.
%% The waiting `receive` lives in `proc_lib:sync_start/2` — a *different module* from this
%% one, i.e. across a `call_ext` boundary.
run() ->
    shape(proc_lib:start(?MODULE, child, [], 1000)).

%% `proc_lib:start_link/4` is the same shape with a link.
startlink() ->
    shape(proc_lib:start_link(?MODULE, child, [], 1000)).

%% The child `proc_lib:start/4` requires: ack the parent, then return.
child() ->
    proc_lib:init_ack({ok, self()}).

%% The same ack with the parent passed **explicitly** (rules out the `$ancestors`
%% process-dictionary lookup as the cause).
child_explicit() ->
    proc_lib:init_ack(self(), {ok, self()}).

%% --- the working controls ----------------------------------------------------

%% Control: the receive is INLINE in the entry function.
inline() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() -> Parent ! {ack, self(), {ok, self()}} end),
    shape(receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    end).

%% The same inline shape, but with a hardcoded `after 2000` (a statically readable
%% delay: the runner arms a real deadline for it).
inline_hard() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() -> Parent ! {ack, self(), {ok, self()}} end),
    shape(receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    after 2000 -> timeout
    end).

%% --- H1: a receive in a NON-ENTRY function frame (same module) ---------------

%% The spawn is here; the `receive` is in `wait/2` below, i.e. one call frame further
%% from the process's entry point — but still in this module's own code.
sep() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() -> Parent ! {ack, self(), {ok, self()}} end),
    shape(wait(Pid, Ref)).

%% H1 + H2: the waiting function also carries a hardcoded `after`.
sep_hard() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() -> Parent ! {ack, self(), {ok, self()}} end),
    shape(wait_hard(Pid, Ref)).

%% --- H1 across MODULES: the receive lives in another module ------------------

%% The discriminated failing shape: the spawning code is here, the waiting `receive` is
%% `plwait:wait/2` — i.e. one `call_ext` away, exactly as `proc_lib:sync_start/2` is from
%% `proc_lib:start/4`.
cross() ->
    {Pid, Ref} = erlang:spawn_monitor(?MODULE, child_ack, [self()]),
    shape(plwait:wait(Pid, Ref)).

%% As above with the hardcoded `after` the cross-module helper also carries.
cross_hard() ->
    {Pid, Ref} = erlang:spawn_monitor(?MODULE, child_ack, [self()]),
    shape(plwait:wait_hard(Pid, Ref)).

%% The non-blocking cross-module control: the module boundary itself is not the problem.
pure() ->
    plwait:plain(41).

%% The child the two `cross` shapes spawn: ack the parent with `!`.
child_ack(Parent) ->
    Parent ! {ack, self(), {ok, self()}}.

%% --- the exact proc_lib structure, reproduced locally ------------------------

%% `proc_lib:start/4` is `sync_start(spawn_mon(?MODULE, init_p, [Parent,Ancestors,M,F,A]), T)`,
%% so the waiting `receive` lives in a *separate* function that takes `{Pid, Ref}` and
%% `Timeout` as arguments. This is that shape with no `proc_lib` involved at all — and it
%% lives entirely in this module, which is why it works.
plshaped() ->
    shape(plsync(plspawn_mon(?MODULE, child_ack2, []), 1000)).

plsync({Pid, Ref}, Timeout) ->
    receive
        {ack, Pid, Return} ->
            erlang:demonitor(Ref, [flush]),
            Return;
        {'DOWN', Ref, process, Pid, Reason} ->
            {error, Reason}
    after Timeout ->
            exit(Pid, kill),
            {error, timeout}
    end.

%% `proc_lib:spawn_mon/3` verbatim: the parent name and ancestors come from the
%% process dictionary, and the spawn goes through `erlang:spawn_monitor/4` with the
%% `monitor` option.
plspawn_mon(M, F, A) ->
    Parent = myname(),
    Ancestors = [],
    erlang:spawn_monitor(?MODULE, plinit_p, [Parent, Ancestors, M, F, A]).

%% `proc_lib:init_p/5` verbatim: record the ancestors, then apply the child.
plinit_p(Parent, Ancestors, M, F, A) ->
    put('$ancestors', [Parent | Ancestors]),
    apply(M, F, A).

%% The child of the local reproduction: ack from the recorded `$ancestors` head.
child_ack2() ->
    [Parent | _] = get('$ancestors'),
    Parent ! {ack, self(), {ok, self()}}.

%% `proc_lib:get_my_name/0` verbatim: what `refresh_Parent`'s source gives.
myname() ->
    case process_info(self(), registered_name) of
        {registered_name, Name} -> Name;
        _ -> self()
    end.

%% --- the same-module wait functions -----------------------------------------

wait(Pid, Ref) ->
    receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    end.

wait_hard(Pid, Ref) ->
    receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {error, Reason}
    after 2000 -> timeout
    end.
