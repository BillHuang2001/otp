%%% Port-exit fixture for `tests/port_exit.rs`.
%%%
%%% A **closing** port must tell its links and monitors about it, exactly as a dying process
%%% does: a linked process that traps exits gets `{'EXIT', Port, normal}`, every watcher gets
%%% `{'DOWN', Ref, port, Port, normal}` (after the `'EXIT'`, when the same process is both),
%%% and a non-trapping owner gets nothing for the `normal` reason and *survives* rather than
%%% being terminated. Each function here closes a real port and returns the messages that
%%% arrived, **normalized** (`#Port<…>`/`#Ref<…>` differ between two runtimes and between two
%%% runs, so only their shapes are comparable), so the printed value can be compared
%%% byte-for-byte with the oracle's.
%%%
%%% Rebuilt with the system `erlc` and committed as `port_exit.beam`; the recipe is in this
%%% directory's `README.md`. The oracle's own answers (`erl -noshell -pa <dir> -eval
%%% 'io:format("~w~n",[port_exit:trap_owner_mon()]), halt(0).'`) are the expectations the
%%% test pins.
-module(port_exit).
-export([trap_owner/0, trap_owner_mon/0, notrap_owner_mon/0, watcher_only/0,
         connected/0]).
%%% A **trapping** owner gets exactly one `{'EXIT', Port, normal}`.
trap_owner() ->
    process_flag(trap_exit, true),
    Port = open_port({spawn, "true"}, []),
    true = port_close(Port),
    shapes(drain()).

%%% A trapping owner that also **watches** the port gets the `'EXIT'` first and then the
%%% `'DOWN'` — one of each, never twice.
trap_owner_mon() ->
    process_flag(trap_exit, true),
    Port = open_port({spawn, "true"}, []),
    _Reference = monitor(port, Port),
    true = port_close(Port),
    shapes(drain()).

%%% A **non-trapping** owner gets no `'EXIT'` (a `normal` reason is ignored in the link
%%% family) and is not terminated: it gets just the `'DOWN'` and goes on running.
notrap_owner_mon() ->
    process_flag(trap_exit, false),
    Port = open_port({spawn, "true"}, []),
    _Reference = monitor(port, Port),
    true = port_close(Port),
    shapes(drain()).

%%% A **`erlang:port_connect/2`ed** second process grows the port's link set and is told of the
%%% close: `open_port/2` links the owner only, so `erlang:port_info(Port, links)` holds one pid
%%% before the connect and two after it, and the connector — a trapping helper, not the owner —
%%% gets exactly one `{'EXIT', Port, normal}`. `erlang:port_connect/2` answers `true`.
%%%
%%% Returned shape: `{ConnectAnswer, LinksBefore, LinksAfter, Shapes}`.
connected() ->
    Parent = self(),
    Port = open_port({spawn, "true"}, []),
    Before = length(element(2, erlang:port_info(Port, links))),
    Connector = spawn(fun() ->
                              process_flag(trap_exit, true),
                              Answered = erlang:port_connect(Port, self()),
                              Parent ! {connected, Answered},
                              receive go -> Parent ! {signal, shapes(drain())} end
                      end),
    receive {connected, Answered} -> ok end,
    After = length(element(2, erlang:port_info(Port, links))),
    true = port_close(Port),
    Connector ! go,
    Shows = receive
                {signal, S} -> S
            after 5000 -> connector_timeout
            end,
    {Answered, Before, After, Shows}.

%%% A watcher that is **not** linked to the port (a spawned helper) gets just the `'DOWN'`.
watcher_only() ->
    Parent = self(),
    Port = open_port({spawn, "true"}, []),
    Watcher = spawn(fun() ->
                            process_flag(trap_exit, false),
                            _Reference = monitor(port, Port),
                            Parent ! watching,
                            receive
                                go -> Parent ! {watched, shapes(drain())}
                            end
                    end),
    receive watching -> ok end,
    true = port_close(Port),
    Watcher ! go,
    receive
        {watched, Shapes} -> Shapes
    after 5000 -> watcher_timeout
    end.

%%% Everything the mailbox holds once it has gone quiet.
drain() -> drain([]).
drain(Acc) ->
    receive Message -> drain([Message | Acc])
    after 200 -> lists:reverse(Acc)
    end.

%%% The comparable shape of each message: the port and the reference are replaced by the
%%% predicates that identify them, which are all two runtimes can agree on.
shapes(Messages) -> [shape(Message) || Message <- Messages].

shape({'EXIT', Port, Reason}) when is_port(Port) -> {exit, is_port(Port), Reason};
shape({'DOWN', Reference, Kind, Port, Reason}) when is_reference(Reference), is_port(Port) ->
    {down, is_reference(Reference), Kind, is_port(Port), Reason};
shape(Other) -> {other, Other}.
