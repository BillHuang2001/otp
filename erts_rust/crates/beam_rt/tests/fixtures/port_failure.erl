%%
%% The Erlang half of `beam_rt/tests/port_failure.rs` (contract A.4): it opens a real C
%% driver port (`tests/fixtures/port_failure_driver.c`) and returns what the **owner** is left
%% with after the driver fails its own port from inside its `output` callback.
%%
%% The driver's object directory and its registered name are arguments, not literals, so this
%% module is compiled once and reused for every row (only the test knows where `cc` put the
%% `.so`): `{ok,_} = erl_ddll:try_load(Dir, Name, [])` then `open_port({spawn_driver, Name})`.
%%
%% Every printed value is **normalized**: a `#Port<…>` and a `#Ref<…>` cannot be equal across
%% two OS processes (the port number and the reference id are allocated independently per
%% runtime), so each is replaced by the predicate that identifies it (`is_port/1`,
%% `is_reference/1`). The *shape* of each message — tag, kind slot, reason — and their **order**
%% is what the two runtimes must agree on.
-module(port_failure).
-export([trapping/3, trapping_monitored/3, trapping_list_monitored/3, non_trapping/3]).

%% The port a row opens.
open(Dir, Name) ->
    {ok, _} = erl_ddll:try_load(Dir, Name, []),
    open_port({spawn_driver, Name}, [binary]).

%% The same port with the **list** stream option: the surviving read must render as a charlist,
%% so this row pins that the recipient of a failed port's read is its own, option included.
open_list(Dir, Name) ->
    {ok, _} = erl_ddll:try_load(Dir, Name, []),
    open_port({spawn_driver, Name}, []).

shape({'EXIT', P, R}) when is_port(P) -> {exit, true, R};
shape({'DOWN', Rf, port, P, R}) when is_reference(Rf), is_port(P) -> {down, true, true, R};
shape({P, {data, D}}) when is_port(P) -> {data, D};
shape(Other) -> {misc, Other}.

drain(0, Acc) ->
    lists:reverse(Acc);
drain(N, Acc) ->
    receive M -> drain(N - 1, [shape(M) | Acc])
    after 300 -> lists:reverse(Acc)
    end.

%% What the port answers once it is gone: `port_info/1` is `undefined` and a second close is
%% `badarg` — the same pair the oracle answers.
post(P) ->
    {erlang:port_info(P), try erlang:port_close(P) catch C:R -> {C, R} end}.

%% A **trapping** owner: the port's exit signal arrives as a message.
trapping(Dir, Name, Op) ->
    process_flag(trap_exit, true),
    P = open(Dir, Name),
    true = port_command(P, <<Op>>),
    {drain(4, []), post(P)}.

%% A trapping owner that also **watches** the port: the `'EXIT'` and the `'DOWN'` are owed
%% independently, so both must arrive (in that order).
trapping_monitored(Dir, Name, Op) ->
    process_flag(trap_exit, true),
    trapping_monitored_with(fun open/2, Dir, Name, Op).

%% The same, over a `list`-stream port.
trapping_list_monitored(Dir, Name, Op) ->
    process_flag(trap_exit, true),
    trapping_monitored_with(fun open_list/2, Dir, Name, Op).

trapping_monitored_with(Open, Dir, Name, Op) ->
    P = Open(Dir, Name),
    Rf = erlang:monitor(port, P),
    true = port_command(P, <<Op>>),
    {drain(4, []), post(P), erlang:is_reference(Rf)}.

%% A **non-trapping** owner, in a helper process so this one can watch it: a non-`normal`
%% reason terminates it (its `'DOWN'` carries the port's own reason), while `normal` is ignored
%% and the helper lives on to send `alive`.
non_trapping(Dir, Name, Op) ->
    Self = self(),
    {_H, _M} = spawn_monitor(fun() ->
                                     P = open(Dir, Name),
                                     %% Reported **before** the command: a non-`normal` reason
                                     %% kills this owner from inside `port_command/2`, so a send
                                     %% after it would race the kill (it is never reached on the
                                     %% oracle and is reached on a runtime that defers the
                                     %% signal). The row is about termination, not that race.
                                     Self ! {opened, P},
                                     true = port_command(P, <<Op>>),
                                     timer:sleep(400),
                                     Self ! alive
                             end),
    n_drain(3, []).

n_drain(0, Acc) ->
    lists:reverse(Acc);
n_drain(N, Acc) ->
    receive M -> n_drain(N - 1, [n_shape(M) | Acc])
    after 700 -> lists:reverse(Acc)
    end.

n_shape({'DOWN', Rf, process, Pid, R}) when is_reference(Rf), is_pid(Pid) ->
    {helper_down, true, R};
n_shape({opened, P}) when is_port(P) ->
    opened_port;
n_shape(alive) ->
    alive;
n_shape(Other) ->
    {other, Other}.
