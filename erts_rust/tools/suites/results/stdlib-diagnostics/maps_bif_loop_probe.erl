%% Diagnostic probe for class D8 of `results/stdlib-notes.md` §D.
%%
%% Old observation: the `maps_SUITE` cases whose work is a BIF (or a stdlib loop)
%% that keeps calling an Erlang fun — `t_intersect_with`, `t_iterator_2`,
%% `t_merge_with` — return the right result but ~900–1000× later than the oracle
%% (10.1 s / 41.0 s / 31.4 s vs 11 / 47 / 23 ms), i.e. no pre-emption/reduction
%% accounting inside a BIF-driven Erlang loop.
%%
%% This probe runs the same three *shapes* at a reduced size so the whole check
%% finishes quickly, prints a checksum (both sides must compute the same work) and
%% the elapsed microseconds from `erlang:monotonic_time/1`. Only the **ratio**
%% between the two runtimes on the same host is meaningful — the host is shared,
%% so never quote the absolute microseconds.
%%
%% Erlc: `erlc -o . maps_bif_loop_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'maps_bif_loop_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'maps_bif_loop_probe:main(), halt(0).'`
-module(maps_bif_loop_probe).
-export([main/0]).

-define(SIZE, 1000).
-define(ROUNDS, 50).

main() ->
    M1 = map(?SIZE, 0),
    M2 = map(?SIZE, 1),
    run("merge_with", fun() -> maps:merge_with(fun(_K, V1, V2) -> {V1, V2} end, M1, M2) end),
    run("intersect_with", fun() -> maps:intersect_with(fun(_K, V1, V2) -> {V1, V2} end, M1, M2) end),
    run("iterator/next walk", fun() -> walk(M1) end),
    run("to_list (reference)", fun() -> maps:to_list(M1) end),
    ok.

map(N, Offset) ->
    maps:from_list([{I, I + Offset} || I <- lists:seq(1, N)]).

%% `maps:iterator/1` + `maps:next/1`, the `t_iterator_2` shape.
walk(M) ->
    walk_next(maps:next(maps:iterator(M)), 0).

walk_next({K, V, Iter}, Acc) ->
    walk_next(maps:next(Iter), Acc bxor K bxor V);
walk_next(none, Acc) ->
    Acc.

run(Label, F) ->
    %% Warm up once so the first-call JIT/compile cost never lands in the sample.
    _ = checksum(F()),
    T0 = erlang:monotonic_time(microsecond),
    Sum = loop(?ROUNDS, F, 0),
    T1 = erlang:monotonic_time(microsecond),
    io:format("~-22s rounds=~p elapsed_us=~p checksum=~p~n",
              [Label, ?ROUNDS, T1 - T0, Sum]).

loop(0, _F, Acc) ->
    Acc;
loop(N, F, Acc) ->
    loop(N - 1, F, Acc bxor checksum(F())).

%% A small, order-stable number so both runtimes prove they did the same work.
checksum(L) when is_list(L) ->
    length(L);
checksum(T) when is_tuple(T) ->
    erlang:phash2(T);
checksum(M) when is_map(M) ->
    maps:size(M);
checksum(I) when is_integer(I) ->
    I.
