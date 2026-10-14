%%% Self-query driver for `tests/position.rs` (the in-machine BIF door).
%%%
%%% `recv:main/0` is `probe_b(self())` — it asks `erlang:process_info/1,2` for the
%%% four execution-position items (`current_function`, `initial_call`,
%%% `current_location`, `current_stacktrace`) of the process it is *running in*,
%%% which is exactly the query a scheduler pass cannot answer from its runner
%%% state (the executing process's machine is taken out of it for the whole
%%% slice).
%%%
%%% A spawned export process's return value is **not observable in Erlang**, so
%%% this driver reports `recv:main/0`'s answer through ordinary message passing:
%%% `go/1` runs the self-query inside the *scheduled* process and sends the result
%%% to the parent it is given; `sink/0` is the built runtime's parent — it
%%% receives that report and returns it, so the test can read it back through
%%% `Runtime::process_result`. The oracle's parent is the `erl` probe's own root
%%% process, which receives the same message.
%%%
%%% `tests/position.rs` compiles this at test time with the system `erlc` (cwd =
%%% the scratch dir, bare `pos_self.erl`, so the recorded source name is exactly
%%% `"pos_self.erl"`); the fixture is never committed as a `.beam`.
-module(pos_self).
-export([go/1, sink/0]).
go(Parent) ->
    Parent ! {result, recv:main()}.

sink() ->
    receive
        {result, R} -> R
    end.
