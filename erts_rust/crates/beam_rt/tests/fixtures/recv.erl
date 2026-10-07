%%% Execution-position fixture for `tests/position.rs` (erl `process_info/2`'s
%%% `current_function`/`initial_call`/`current_location`/`current_stacktrace`).
%%%
%%% `wait/0`'s clause head sits on line 12 so a parked process reports the
%%% oracle's `{line,12}`; `probe_b/1` reads the four items of a parked process.
%%%
%%% `tests/position.rs` compiles this at test time with the system `erlc` (cwd =
%%% the scratch dir, argument `recv.erl`, so the recorded source name is exactly
%%% `"recv.erl"`); the fixture is never committed as a `.beam`.
-module(recv).
-export([wait/0, probe_b/1, main/0]).
wait() ->
    receive
        _ -> ok
    end.

probe_b(Pid) ->
    process_info(Pid, [current_function, initial_call, current_location, current_stacktrace]).

main() ->
    probe_b(self()).
