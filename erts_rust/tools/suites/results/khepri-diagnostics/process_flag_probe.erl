%% Diagnostic probe for RC-B of `results/khepri-notes.md`.
%%
%% `ra_log_wal:init/1` sets three scheduler/heap flags that this runtime answers
%% with `badarg` instead of the previous value, so every `ra` system fails to
%% start once RC-A (the `dets` write) is worked around.
%%
%% Erlc: `erlc -o . process_flag_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'process_flag_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'process_flag_probe:main(), halt(0).'`
-module(process_flag_probe).
-export([main/0]).

main() ->
    Flags = [{min_heap_size, 233},
             {min_bin_vheap_size, 46422},
             {fullsweep_after, 0},
             {max_heap_size, #{size => 0}},
             {save_calls, 5},
             {message_queue_data, off_heap},
             {priority, high},
             {trap_exit, true}],
    lists:foreach(fun({Flag, Value}) ->
                          Name = io_lib:format("process_flag(~p, ~p)", [Flag, Value]),
                          io:format("~-46s => ~p~n", [lists:flatten(Name), catch process_flag(Flag, Value)])
                  end, Flags),
    ok.
