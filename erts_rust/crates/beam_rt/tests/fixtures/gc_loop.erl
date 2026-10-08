-module(gc_loop).
-export([loop/1, alloc/1]).
loop(0) -> 0;
loop(N) when is_integer(N), N > 0 ->
    L = build(N),
    _ = erlang:length(L),
    loop(N - 1) + 1.
build(0) -> [];
build(N) -> [{N, N + 1, [N, N + 2]} | build(N - 1)].
alloc(N) -> _ = build(N), N.
