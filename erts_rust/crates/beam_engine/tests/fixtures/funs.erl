% Fun / closure fixture for `tests/funs.rs`.
%
% Exercises the `funs` instruction family the engine implements:
%   * `make_fun3`   — building an anonymous closure (`test/0`, `capture/0`)
%   * `call_fun2`   — a local higher-order function calling a fun (`apply_twice/2`)
%   * `apply`       — a dynamic `Mod:Fun(..)` whose Mod/Fun are not static atoms
%   * `apply_last`  — the same dynamic call in tail position
%
% The closure bodies call `erlang:'+'/2`, which the test context supplies through
% its BIF hook; that also interns the `erlang` and `'+'` atoms this module needs
% to name the apply target.
-module(funs).

-export([apply_twice/2, test/0, adder/1, capture/0, dynamic/4, tail_dynamic/4]).

% Local higher-order function: applies `F` twice. `F` is an opaque parameter, so
% the compiler emits `call_fun2 {atom,unsafe} 1 {x,1}`.
apply_twice(F, X) -> F(F(X)).

% Builds an anonymous closure (no free variables) and hands it to the local
% higher-order function.
test() -> apply_twice(fun(X) -> X + 1 end, 5).

% Builds a closure that captures the free variable `N`.
adder(N) -> fun(X) -> X + N end.

% Uses a captured-variable closure.
capture() ->
    Add = adder(10),
    Add(5).

% Dynamic `Mod:Fun(A, B)` — `Mod`/`Fun` are not statically known atoms, so the
% compiler emits `apply 2` (non-tail; the result is captured in a tuple).
dynamic(Mod, Fun, A, B) -> {ok, Mod:Fun(A, B)}.

% Dynamic `Mod:Fun(A, B)` in tail position -> `apply_last 2 0`.
tail_dynamic(Mod, Fun, A, B) -> Mod:Fun(A, B).
