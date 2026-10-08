-module(gc_ints).
-export([loop/2, l1/1]).
loop(0, Acc) -> Acc;
loop(N, Acc) when is_integer(N), N > 0 ->
    loop(N - 1, (Acc + N) rem 1000003).
l1(N) -> loop(N, 0).
