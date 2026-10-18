%% The BIF-result copy-out's end-to-end witness (`tests/foreign_root_loop.rs`).
%%
%% `hold/1` keeps the 3-tuple `erlang:statistics(garbage_collection)` returns live as its own
%% accumulator throughout. The copy-out moves an `Ok` container result onto the **running
%% process's own young heap** (`src/context/calls.rs::call_registered`), so the loop allocates
%% the tuple's 4 words (header + 3 small integers) here every iteration: 13 words of `junk` +
%% 4 = 17 per iteration against a 233-word nursery, i.e. one collection every 13 iterations.
%% The tuple is an ordinary scanned root: the loop must keep collecting at that fill law.
%%
%% `junk/1` is what gives each iteration a heap-admission point at all: it builds a fresh
%% heap-resident term (a 3-tuple holding a 3-element list and a 2-tuple) out of the runtime
%% value `N`, and the loop throws it away. It is an argument of the tail call, so no compiler
%% pass can discard it.
%%
%% `control/1` is the same loop reading only `element(1, ...)` — a small integer — so it pays
%% the same copy-out but retains no container: identical collections, far fewer fullsweeps.
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
