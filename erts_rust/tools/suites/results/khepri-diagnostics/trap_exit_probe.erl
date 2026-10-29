%% Diagnostic probe for RC-B (the `trap_exit` / `spawn_opt` half) of
%% `results/khepri-notes.md`.
%%
%% Two questions.
%% (1) `process_flag(trap_exit, true)` returns the *previous* value. A probe run
%%     under `-noshell -eval` was reported to answer `false` on the Rust runtime
%%     and `true` on the oracle. The checks below separate
%%       (a) the initial flag state of the process that runs `-eval`
%%           (`erlang:process_info(self(), trap_exit)`, no BIF call needed),
%%       (b) what `process_flag(trap_exit, …)` answers in that same process,
%%       (c) the same two checks inside a freshly `spawn/1`ed process, whose
%%           initial state has no CLI involvement at all,
%%       (d) the full `ra_log_wal:init/1` preface applied in order to one
%%           spawned process (ra/src/ra_log_wal.erl:308-314).
%% (2) `spawn_opt/4` is the other way a process is born with flags
%%     (`proc_lib` passes a caller-supplied `{spawn_opt, …}` straight through),
%%     so the heap-sizing birth options are checked too.
%%
%% Erlc: `erlc -o . trap_exit_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'trap_exit_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'trap_exit_probe:main(), halt(0).'`
-module(trap_exit_probe).
-export([main/0]).

main() ->
    show("[eval process] process_info(self(), trap_exit)",
         caught(fun() -> element(2, process_info(self(), trap_exit)) end)),
    show("[eval process] process_flag(trap_exit, true)",
         caught(fun() -> process_flag(trap_exit, true) end)),
    show("[eval process] process_flag(trap_exit, false)",
         caught(fun() -> process_flag(trap_exit, false) end)),
    show("[eval process] process_flag(trap_exit, false) again",
         caught(fun() -> process_flag(trap_exit, false) end)),
    %% a plain spawn: no CLI path, no init process, initial flags only.
    show("[spawned process] process_info(self(), trap_exit)",
         in_child(fun() -> element(2, process_info(self(), trap_exit)) end)),
    show("[spawned process] process_flag(trap_exit, true)",
         in_child(fun() -> process_flag(trap_exit, true) end)),
    %% the full ra_log_wal:init/1 preface on one process, in order
    show("ra_log_wal preface in one spawned process",
         in_child(fun() ->
                          [{trap_exit, process_flag(trap_exit, true)},
                           {message_queue_data,
                            process_flag(message_queue_data, off_heap)},
                           {min_bin_vheap_size,
                            process_flag(min_bin_vheap_size, 46422)},
                           {min_heap_size,
                            process_flag(min_heap_size, 233)}]
                  end)),
    %% spawn_opt birth options (the proc_lib / gen_server path).
    show("spawn_opt [{min_heap_size, 233}] process_info(self(), min_heap_size)",
         in_child_opt([{min_heap_size, 233}],
                      fun() -> element(2,
                                       process_info(self(), min_heap_size)) end)),
    show("spawn_opt [{fullsweep_after, 0}] + process_info(fullsweep_after)",
         in_child_opt([{fullsweep_after, 0}],
                      fun() -> element(2,
                                       process_info(self(), fullsweep_after)) end)),
    show("spawn_opt [{min_bin_vheap_size, 46422}] + process_info",
         in_child_opt([{min_bin_vheap_size, 46422}],
                      fun() -> element(2,
                                       process_info(self(),
                                                    min_bin_vheap_size)) end)),
    ok.

in_child(Fun) ->
    Parent = self(),
    Pid = spawn(fun() -> Parent ! {self(), caught(Fun)} end),
    receive {Pid, Result} -> Result after 30000 -> timeout end.

in_child_opt(Opts, Fun) ->
    Parent = self(),
    Pid = spawn_opt(fun() -> Parent ! {self(), caught(Fun)} end, Opts),
    receive {Pid, Result} -> Result after 30000 -> timeout end.

caught(Fun) ->
    try Fun() of
        Value -> {ok, Value}
    catch
        Class:Reason -> {Class, Reason}
    end.

show(Name, Value) ->
    io:format("~-74s => ~p~n", [Name, Value]).
