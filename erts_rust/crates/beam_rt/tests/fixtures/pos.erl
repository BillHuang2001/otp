%% Execution-position probe (erlang:process_info/2's `current_function`,
%% `initial_call`, `current_location` and `current_stacktrace`): it must run
%% **as a scheduled process** so the runtime can read another process's engine.
-module(pos).
-export([probe/1, keys/1]).

probe(Pid) ->
    [process_info(Pid, current_function),
     process_info(Pid, initial_call),
     process_info(Pid, current_location),
     process_info(Pid, current_stacktrace)].

keys(Pid) ->
    [element(1, Entry) || Entry <- process_info(Pid)].
