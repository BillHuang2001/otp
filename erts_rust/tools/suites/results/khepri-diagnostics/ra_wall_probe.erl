%% Diagnostic probe for the Khepri wall behind RC-A/RC-B (see
%% `results/khepri-notes.md` §1/§2).
%%
%% With RC-A fixed, every `ra`-backed Khepri module dies in the *fixture*
%% (`helpers:start_ra_system/1`). This probe reproduces that fixture exactly —
%% `ra_system:start/1` on `ra_system:default_config/0` with `wal_max_size_bytes
%% => 16 * 1024` — and prints the failure at a depth that names the child that
%% died, then narrows it to the `ra_log_wal:init/1` flag preface
%% (ra/src/ra_log_wal.erl:308-314) called on one process.
%%
%% Erlc: `erlc -o . ra_wall_probe.erl`
%% Run:  `beam_rt -noshell <khepri -pa…> -pa . -eval 'ra_wall_probe:main(), halt(0).'`
%%       `erl     -noshell <khepri -pa…> -pa . -eval 'ra_wall_probe:main(), halt(0).'`
%% (run each side in its own throwaway cwd: the probe creates a data dir)
-module(ra_wall_probe).
-export([main/0]).

main() ->
    show("application:ensure_all_started(ra)",
         caught(fun() -> application:ensure_all_started(ra) end)),
    Dir = "ra_wall_probe_store",
    _ = file:del_dir_r(Dir),
    Config = (ra_system:default_config())#{name => wall_probe_system,
                                           data_dir => Dir,
                                           wal_data_dir => Dir,
                                           wal_max_size_bytes => 16 * 1024,
                                           names =>
                                               ra_system:derive_names(
                                                 wall_probe_system)},
    io:format("~-52s => ~P~n",
              ["ra_system:start/1 (the khepri fixture call)",
               caught(fun() -> ra_system:start(Config) end),
               24]),
    show("ra_log_wal:init/1 flag preface, per call",
         in_child(fun() ->
                          [{trap_exit, process_flag(trap_exit, true)},
                           {message_queue_data,
                            process_flag(message_queue_data, off_heap)},
                           {min_bin_vheap_size,
                            caught(fun() ->
                                           process_flag(min_bin_vheap_size,
                                                        46422)
                                   end)},
                           {min_heap_size,
                            caught(fun() ->
                                           process_flag(min_heap_size, 233)
                                   end)}]
                  end)),
    show("process_flag(fullsweep_after, 0)",
         in_child(fun() -> process_flag(fullsweep_after, 0) end)),
    _ = file:del_dir_r(Dir),
    ok.

in_child(Fun) ->
    Parent = self(),
    Pid = spawn(fun() -> Parent ! {self(), caught(Fun)} end),
    receive {Pid, Result} -> Result after 60000 -> timeout end.

caught(Fun) ->
    try Fun() of
        Value -> {ok, Value}
    catch
        Class:Reason -> {Class, Reason}
    end.

show(Name, Value) ->
    io:format("~-52s => ~p~n", [Name, Value]).
