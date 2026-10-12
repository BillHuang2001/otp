%% The cross-module callee of `xmod_caller:run/1` (see ../raise_stacktrace.rs).
%% Compiled with `erlc +deterministic` (README.md).
-module(xmod_callee).
-export([boom/1]).

boom(0) -> 0;
boom(_) -> erlang:error(badarg).
