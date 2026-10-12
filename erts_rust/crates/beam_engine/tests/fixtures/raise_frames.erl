%% Uncaught-raise stacktrace oracle fixture (see ../raise_stacktrace.rs).
%%
%% Compiled with the system `erlc +deterministic` from a neutral directory (see
%% README.md) and committed as `raise_frames.beam`.
%%
%% `outer/1` makes a **non-tail** call into `boom/1`, so when `boom/1` raises the
%% caller frame is still live and the engine's uncaught-raise frames must name
%% `raise_frames:boom/1` first, then `raise_frames:outer/1`.
%%
%% The `boom(0)` clause keeps the call non-tail: without it the compiler proves
%% `boom/1` always raises and folds `outer/1` into a tail call (no live frame).
-module(raise_frames).
-export([outer/1, boom/1]).

outer(X) -> boom(X) + 1.

boom(0) -> 0;
boom(_) -> erlang:error(badarg).
