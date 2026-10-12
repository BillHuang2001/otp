%% Catch-stacktrace oracle fixture (see ../exceptions.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic catchtrace.erl`.
%%
%% The shapes the caught-`S` engine path must reproduce:
%%   * `run/0`  — the raising call is INLINE in the `try` body, so the head
%%                frame of the cooked `S` names `catchtrace:run/0` itself.
%%   * `run2/0` — the raising call is a NON-TAIL local call to `raise_badarg/0`,
%%                so the cooked `S` runs the raising function first and its live
%%                caller (`run2/0`) second, innermost-first.
%%
%% Both raise through `erlang:error(badarg)` — a `call_ext` to a BIF, which the
%% engine turns into a class-`error` `badarg` raise that lands in the catch frame
%% and rebuilds the machine's pre-unwind frame snapshot.
-module(catchtrace).
-export([run/0, run2/0]).

run() ->
    try erlang:error(badarg) catch _C:_R:S -> S end.

run2() ->
    try raise_badarg() catch _C:_R:S -> S end.

raise_badarg() ->
    erlang:error(badarg).
