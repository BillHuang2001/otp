%% Cross-module uncaught-raise stacktrace oracle fixture (see
%% ../raise_stacktrace.rs). Compiled with `erlc +deterministic` (README.md).
%%
%% `run/1` makes a **non-tail** cross-module call into `xmod_callee:boom/1`, so the
%% engine enters the callee in-machine and, when the callee raises, the uncaught
%% frames must name `xmod_callee:boom/1` first, then `xmod_caller:run/1`.
-module(xmod_caller).
-export([run/1]).

run(X) -> xmod_callee:boom(X) + 1.
