-module(slice).
-export([add/2, fib/1, pick/1]).

add(A, B) -> A + B.

fib(N) -> fib(N, 0, 1).

fib(0, A, _B) -> A;
fib(N, A, B) -> fib(N-1, B, A+B).

pick(a) -> 1;
pick(_) -> 2.
