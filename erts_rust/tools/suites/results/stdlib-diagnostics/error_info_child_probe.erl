%% Diagnostic probe for class D1 of `results/stdlib-notes.md` §D — the *suite-level*
%% repro the notes recorded:
%%
%%   erlc -o $TMPDIR/probe/ebin lib/stdlib/test/binary_module_SUITE.erl
%%   cd $TMPDIR/probe/cwd && <rt> -noshell -pa $TMPDIR/probe/ebin -pa <staged>/ebin \
%%     -eval 'P = spawn_monitor(fun() -> binary_module_SUITE:error_info([]) end), …
%%
%% Old observation: oracle `{normal,normal}` with 464 lines of output; rust either
%% the child dying with `{function_clause,[]}` after part of the output, or the
%% **whole runtime aborting** with `beam_rt: system_limit` (exit 1). Calling
%% `error_info([])` in the main process was fine on rust.
%%
%% The suite prints ~460 lines itself (that IS the case's output, one per BIF
%% error-info entry), so this probe only adds the two summary lines: the main
%% process's result shape and the child's `DOWN` reason. Count the printed lines
%% with `wc -l` if you want the depth.
%%
%% Prerequisites: `binary_module_SUITE` and its helpers compiled with the oracle's
%% `erlc` into a directory on the code path (the `suites` driver stages them under
%% `$TMPDIR/erts-suites-stage/stdlib/{ebin,src}`).
%%
%% Erlc: `erlc -o . error_info_child_probe.erl`
%% Run:  `beam_rt -noshell -pa . -pa <staged>/ebin \
%%          -eval 'error_info_child_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -pa <staged>/ebin \
%%          -eval 'error_info_child_probe:main(), halt(0).'`
-module(error_info_child_probe).
-export([main/0]).

main() ->
    io:format("~n-- main process~n"),
    Res = try binary_module_SUITE:error_info([]) of
              V -> {ok, V}
          catch
              C:R -> {caught, C, first(R)}
          end,
    io:format("~n  main result=~p~n", [Res]),
    io:format("-- spawn_monitor child~n"),
    child(),
    io:format("DONE~n").

child() ->
    {Pid, Ref} = spawn_monitor(fun() -> binary_module_SUITE:error_info([]) end),
    receive
        {'DOWN', Ref, process, Pid, Why} ->
            io:format("~n  DOWN=~p~n", [Why])
    after 600000 ->
              exit(Pid, kill),
              io:format("~n  TIMEOUT~n")
    end.

first({Reason, _Args}) when is_atom(Reason) -> Reason;
first(R) -> R.
