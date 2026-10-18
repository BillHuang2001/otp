% Fun / closure proof fixture for `tests/real_beam_variant_proofs/funs.rs`.
%
% Every export is ordinary, idiomatic Erlang whose *compiler output* is the
% instruction under proof — the point of the suite is that the shapes come from
% the OTP compiler, never from a hand-built module:
%
%   * `make_fun3`   — `add_n/1` (one free variable), `env2/2` (two)
%   * `call_fun`    — `apply1/2` and the first call in `twice/2`
%   * `call_fun2`   — the second call in `twice/2`; and, cross-module, the
%                     `call_fun2` inside the real stdlib `lists:map/2` that
%                     `map_with/2` hands its fun to
%   * `apply`       — `dyn/4` (a dynamic `M:F(A, B)` in non-tail position)
%   * `apply_last`  — `dyn_tail/4` (the same call in tail position)
%
% The closure bodies call `erlang:'+'/2`; `sum2/2` is the *module* target the
% `apply` rows name dynamically, so an `apply` reaches real compiled code as well
% as (in the `apply_last` row) a BIF.
-module(vb_funs).

-export([add_n/1, env2/2, sum2/2, apply1/2, twice/2, dyn/4, dyn_tail/4, map_with/2]).

% A closure capturing one free variable: `make_fun3` with a one-element env.
add_n(N) -> fun(X) -> X + N end.

% A closure capturing two free variables: `make_fun3` with a two-element env.
env2(A, B) -> fun(X) -> X + A + B end.

% The statically-known module function a dynamic `apply` names at run time.
sum2(A, B) -> A + B.

% A higher-order function whose fun parameter is unknown: `call_fun 1`.
apply1(F, X) -> F(X).

% Two calls on such a parameter: `call_fun 1`, then `call_fun2 {atom,safe} 1`.
twice(F, X) -> F(F(X)).

% Dynamic `M:F(A, B)` whose `M`/`F` are not statically known atoms -> `apply 2`.
dyn(M, F, A, B) -> {ok, M:F(A, B)}.

% The same dynamic call in tail position -> `apply_last 2 0`.
dyn_tail(M, F, A, B) -> M:F(A, B).

% Hands an anonymous fun to the real `lists:map/2`, so the stdlib module's own
% compiled `call_fun2` is what applies it (`call_ext_only` + cross-module fun
% resolution).
map_with(F, L) -> lists:map(F, L).
