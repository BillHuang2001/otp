%% Diagnostic probe for the `process blocked (no message available)` wall that
%% the 20 blocked Khepri eunit rows share (see `FINDINGS.md` §"RC-E").
%%
%% The sweep's rust side dies with the typed `beam_rt` capability gap
%% `opcode: run: process blocked (no message available)` on every module whose
%% fixture is `test_ra_server_helpers:setup/1` + `cleanup/1`. This probe drives
%% that same fixture loop by hand — one Ra system per iteration, a couple of
%% Khepri commands, then the *stepwise* cleanup (`khepri_cluster:nodes/1`,
%% `rpc:call(Node, khepri, stop, …)`, `rpc:call(Node, helpers, stop_ra_system,
%% …)`) — and prints a checkpoint to stderr before every step, so the last line
%% printed names the operation the runtime blocked in.
%%
%% `main/0`   — 12 fixtures, checkpointed (what the wall is)
%% `loop/1`   — the same loop with an explicit iteration count
%% `steps/0`  — one fixture, every sub-step checkpointed (the narrowest view)
%%
%% Erlc: `erlc -o . block_wall_probe.erl`
%% Run:  `beam_rt -noshell <khepri -pa…> -pa . -eval 'block_wall_probe:main(), halt(0).'`
%%       `erl     -noshell <khepri -pa…> -pa . -eval 'block_wall_probe:main(), halt(0).'`
%% (run each side in its own throwaway cwd: every iteration creates a data dir)
-module(block_wall_probe).
-export([main/0, loop/1, steps/0, eunit_here/1, eunit_spawned/1, eunit_twice/1,
         each_test/1, leak/1, census/0, eunit_gen/1]).

-define(ITERATIONS, 12).

%% `leak/1` is `loop/1` with a process census after every fixture: if the
%% fixture leaves processes behind, the count climbs one fixture at a time and
%% the block lands once the leftovers cross a threshold.
leak(N) ->
    _ = application:ensure_all_started(khepri),
    lists:foreach(
      fun(I) ->
              iteration(I),
              census()
      end, lists:seq(1, N)),
    ck("leak: ~p fixtures completed", [N]),
    ok.

census() ->
    Procs = processes(),
    ck("census: process_count=~p registered=~p",
       [length(Procs), length(registered())]),
    ok.

%% `each_test/1` runs every `*_test_/0` generator of `Module` as its own
%% `eunit:test({Module, Function}, …)` on the nested route, with an unbuffered
%% stderr checkpoint before each one — the `[verbose]` lines eunit prints on
%% stdout are *buffered* through the io device, so the last line a blocked run
%% manages to print is not where it blocked.
each_test(Module) ->
    Names = [F || {F, 0} <- Module:module_info(exports), is_test_name(F)],
    ck("each_test(~p): ~p generators", [Module, length(Names)]),
    lists:foreach(
      fun(F) ->
              ck("each_test(~p): start ~p", [Module, F]),
              R = eunit:test({Module, F}, [verbose]),
              ck("each_test(~p): ~p => ~p", [Module, F, R])
      end, Names),
    ck("each_test(~p): all done", [Module]),
    ok.

%% `eunit_gen(N)` runs `block_wall_khepri:fixtures(N)` on the nested route, so
%% N can be bisected without recompiling the fixture module.
eunit_gen(N) ->
    R = eunit:test(block_wall_khepri:fixtures(N), [verbose]),
    io:format(standard_error, "eunit_gen(~p) => ~p~n", [N, R]),
    R.

is_test_name(F) ->
    S = atom_to_list(F),
    lists:suffix("_test_", S) orelse lists:suffix("_test", S).

%% `eunit_here/1` is the sweep's own drive — `eunit:test(Module, [verbose])` in
%% the `-eval` process, i.e. through the tree-walking evaluator's cross-module
%% call (a *nested* machine the evaluator pumps). `eunit_spawned/1` runs the
%% identical drive in a **scheduled** process instead, which is the control arm:
%% if only `eunit_here/1` blocks, the wall belongs to the nested-run pump and not
%% to the suite's own processes.
eunit_here(Module) ->
    R = eunit:test(Module, [verbose]),
    io:format(standard_error, "eunit_here(~p) => ~p~n", [Module, R]),
    R.

eunit_spawned(Module) ->
    Self = self(),
    Pid = spawn(fun() ->
                        R = eunit:test(Module, [verbose]),
                        Self ! {self(), R}
                end),
    receive
        {Pid, R} ->
            io:format(standard_error, "eunit_spawned(~p) => ~p~n", [Module, R]),
            R
    after 600000 ->
            io:format(standard_error, "eunit_spawned(~p) => timeout~n", [Module]),
            timeout
    end.

%% The module twice in the same VM: the second pass reuses the atom/ETS/process
%% churn the first one left behind.
eunit_twice(Module) ->
    _ = eunit_spawned(Module),
    eunit_spawned(Module).

main() ->
    loop(?ITERATIONS).

loop(N) ->
    _ = application:ensure_all_started(khepri),
    lists:foreach(fun(I) -> iteration(I) end, lists:seq(1, N)),
    io:format(standard_error, "loop/~p: all ~p fixtures completed~n", [?FUNCTION_NAME, N]),
    ok.

steps() ->
    _ = application:ensure_all_started(khepri),
    iteration(1),
    ok.

iteration(I) ->
    Name = list_to_atom("block_wall_" ++ integer_to_list(I)),
    ck("iter ~p: test_ra_server_helpers:setup/1", [I]),
    Props = test_ra_server_helpers:setup(Name),
    StoreId = maps:get(store_id, Props),
    ck("iter ~p: setup returned store_id ~p", [I, StoreId]),
    ck("iter ~p: khepri:create/3 => ~p",
       [I, catch khepri:create(StoreId, [foo], I)]),
    ck("iter ~p: khepri:get/2 => ~p", [I, catch khepri:get(StoreId, [foo])]),
    ck("iter ~p: khepri:count/2 => ~p", [I, catch khepri:count(StoreId, [foo])]),
    ck("iter ~p: khepri_cluster:nodes/1", [I]),
    Nodes = case khepri_cluster:nodes(StoreId) of
                {ok, L} -> L;
                {error, _} -> [node()]
            end,
    ck("iter ~p: nodes => ~p", [I, Nodes]),
    lists:foreach(
      fun(Node) ->
              ck("iter ~p: rpc:call(~p, khepri, stop, [StoreId])", [I, Node]),
              R1 = rpc:call(Node, khepri, stop, [StoreId]),
              ck("iter ~p: khepri:stop => ~p", [I, R1]),
              ck("iter ~p: rpc:call(~p, helpers, stop_ra_system, [Props])",
                 [I, Node]),
              R2 = rpc:call(Node, helpers, stop_ra_system, [Props]),
              ck("iter ~p: stop_ra_system => ~p", [I, R2])
      end, Nodes),
    ck("iter ~p: done", [I]),
    ok.

%% Every checkpoint goes to stderr, unbuffered through `io:format/2`'s handler,
%% so it is on the terminal even when the runtime aborts the run right after it.
ck(Format, Args) ->
    io:format(standard_error, "CK " ++ Format ++ "~n", Args).
