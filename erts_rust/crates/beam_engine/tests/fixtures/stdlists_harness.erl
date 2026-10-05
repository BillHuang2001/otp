%% The stdlib-harness fixture for `tests/stdlib_lists.rs`.
%%
%% Every export is a thin wrapper around a *real system stdlib* function, so
%% running the wrapper through the engine proves the widened typed-IR
%% interpreter drives the actual `lists.beam` shipped with the system Erlang
%% (OTP 29 / `erts-17.1`, the differential oracle) — not a hand-written mock.
%%
%% `map_double/1` and `filter_pos/1` additionally pass an *anonymous fun* into
%% `lists:map/2` / `lists:filter/2`, so they exercise `make_fun3` + `call_fun2`
%% + cross-module fun resolution (the engine's `ExecutionContext::apply_fun`).
%%
%% Compiled from a neutral directory (see `README.md`) so no worktree path is
%% baked into the committed `.beam`.
-module(stdlists_harness).

-export([reverse/1, seq/2, map_double/1, sort/1, filter_pos/1]).

reverse(L) -> lists:reverse(L).

seq(F, T) -> lists:seq(F, T).

map_double(L) -> lists:map(fun(X) -> X * 2 end, L).

sort(L) -> lists:sort(L).

filter_pos(L) -> lists:filter(fun(X) -> X > 0 end, L).
