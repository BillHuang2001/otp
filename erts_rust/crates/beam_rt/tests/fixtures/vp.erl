%% The unscanned-root valve's end-to-end witness (`tests/foreign_root_loop.rs`).
%%
%% `hold/1` keeps the 3-tuple `erlang:statistics(garbage_collection)` returns live as its
%% own accumulator for the whole loop. That tuple is built by a `RuntimeHooks`-served BIF, so
%% it lives on the pooled BIF heap — a region the copying collector does not scan — and is
%% therefore the foreign container the valve has to classify at every heap-admission point
%% the loop reaches. Its three elements are small integers, so it forwards **no** word a
%% relocation would have to follow or update: it is inert, and the loop must keep collecting.
%%
%% `junk/1` is what gives each iteration a heap-admission point at all: it builds a fresh
%% 401-word-block-free but genuinely heap-resident term (a 3-tuple holding a 3-element list
%% and a 2-tuple) out of the runtime value `N`, and the loop throws it away. It is an
%% argument of the tail call, so no compiler pass can discard it.
%%
%% `control/1` is the same loop reading only `element(1, ...)` — a small integer — so no
%% foreign container is ever a root. It is the baseline the `hold/1` counts are read against.
-module(vp).

-export([hold/1, control/1]).

hold(N) ->
    hold(N, {0, 0, 0}, 0).

hold(0, {A, _B, _C}, _Junk) ->
    {A, heap()};
hold(N, _Stats, _Junk) ->
    hold(N - 1, erlang:statistics(garbage_collection), junk(N)).

control(N) ->
    control(N, 0, 0).

control(0, G, _Junk) ->
    {G, heap()};
control(N, _G, _Junk) ->
    control(N - 1, element(1, erlang:statistics(garbage_collection)), junk(N)).

%% A fresh, runtime-valued term per iteration — abandoned by the next iteration.
junk(N) ->
    {N, [N, N + 1, N + 2], {N, N + 1}}.

heap() ->
    element(2, process_info(self(), heap_size)).
