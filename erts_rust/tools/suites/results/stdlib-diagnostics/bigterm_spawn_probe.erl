%% Diagnostic probe for class D1 of `results/stdlib-notes.md` §D.
%%
%% Old observation: the rust side of `binary_module_SUITE` DIED mid-suite
%% (`beam_rt: system_limit`, exit 1) and only when the work ran in a *spawned
%% child*; `lists:duplicate(1 bsl 21, x)` was the reduced repro. The recorded fix
%% was a need-aware per-depth BIF-region reservation.
%%
%% This probe re-runs the repro in the main process AND in a `spawn_monitor` child
%% (the old failure only hit a spawned child), then a deep-recursion + big-term
%% loop shaped like the suite's `error_info/1`.
%%
%% `erts_debug:flat_size/1` is the *other* thing that used to sit on this path, and
%% it has a size threshold of its own (see `flat_size_gap/0`), so it is measured
%% separately here and never on the main path.
%%
%% Erlc: `erlc -o . bigterm_spawn_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'bigterm_spawn_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'bigterm_spawn_probe:main(), halt(0).'`
-module(bigterm_spawn_probe).
-export([main/0]).

-define(ITERATIONS, 5).

main() ->
    io:format("-- plain process~n"),
    plain(?ITERATIONS),
    io:format("-- spawn_monitor child~n"),
    child(?ITERATIONS),
    io:format("-- deep recursion + big terms~n"),
    deep(?ITERATIONS),
    io:format("-- erts_debug:flat_size/1 threshold~n"),
    flat_size_gap(),
    io:format("DONE~n").

plain(N) ->
    lists:foreach(fun(I) -> report(I, dup()) end, lists:seq(1, N)).

child(N) ->
    Parent = self(),
    {Pid, Ref} =
        spawn_monitor(
          fun() ->
                  lists:foreach(fun(I) ->
                                        Res = try dup() of
                                                  V -> V
                                              catch
                                                  C:R -> {raised, C, R}
                                              end,
                                        Parent ! {iter, I, Res}
                                end,
                                lists:seq(1, N)),
                  Parent ! {child_done, N}
          end),
    collect(Pid, Ref, N).

collect(Pid, Ref, N) ->
    receive
        {iter, I, Res} ->
            io:format("  iter ~p: ~p~n", [I, Res]),
            collect(Pid, Ref, N);
        {child_done, N} ->
            receive
                {'DOWN', Ref, process, Pid, Why} ->
                    io:format("  child DOWN: ~p~n", [Why])
            end;
        {'DOWN', Ref, process, Pid, Why} ->
            io:format("  child DOWN before finishing: ~p~n", [Why])
    after 120000 ->
              exit(Pid, kill),
              io:format("  child TIMEOUT~n")
    end.

report(I, {ok, Len}) ->
    io:format("  iter ~p: len=~p~n", [I, Len]);
report(I, Other) ->
    io:format("  iter ~p: FAILED ~p~n", [I, Other]).

%% The repro from the notes: a 2^21-element list per iteration.
dup() ->
    L = lists:duplicate(1 bsl 21, x),
    {ok, length(L)}.

%% A bounded version of the suite's `error_info/1` shape: a deep recursion that
%% keeps building and discarding large terms, so the heap grows and shrinks at
%% many depths (each depth re-reserves its BIF region).
deep(N) ->
    lists:foreach(fun(I) ->
                          {ok, Depth, Sz} = loop(400, 0, 0),
                          io:format("  iter ~p: depth=~p bytes=~p~n", [I, Depth, Sz])
                  end,
                  lists:seq(1, N)).

loop(0, Depth, Sz) ->
    {ok, Depth, Sz};
loop(K, Depth, Sz) ->
    Bin = binary:copy(<<255>>, 4096),
    IoList = [Integer || Integer <- lists:seq(1, 200)],
    Flat = lists:flatten(io_lib:format("~p", [IoList])),
    NewSz = Sz + byte_size(Bin) + length(Flat),
    loop(K - 1, Depth + 1, NewSz).

%% `erts_debug:flat_size/1` answers a typed `unsupported_bif` for a large enough
%% term (it is fine at 500 000 elements). Recorded separately because it is what
%% made the *first* version of this probe abort, not the `lists:duplicate/2` path.
flat_size_gap() ->
    lists:foreach(fun(N) ->
                          Res = try erts_debug:flat_size(lists:duplicate(N, x)) of
                                    V -> {ok, V}
                                catch
                                    C:R -> {raised, C, R}
                                end,
                          io:format("  flat_size(duplicate(~p)): ~p~n", [N, Res])
                  end,
                  [1000, 100000, 500000, 600000, 1000000]).
