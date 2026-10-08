-module(gc_floats).
-export([loop/2, l1/1]).
loop(0, Acc) -> Acc;
loop(N, Acc) when is_integer(N), N > 0 ->
    loop(N - 1, Acc + math:sqrt(float(N))).
l1(N) -> loop(N, 0.0).
