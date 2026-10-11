%% Oracle fixture for the `call_ext*`-encoded `erlang:apply/3` seam
%% (`beam_engine/tests/call_ext_apply.rs`).
%%
%% `run/3` is a *runtime-valued* `erlang:apply/3`: because `M`/`F`/`A` are
%% parameters the compiler cannot fold them into a direct call, so the body
%% lowers to `call_ext_only 3 {extfunc,erlang,apply,3}` (verified with
%% `beam_disasm:file/1`). The callee the test applies may block in a `receive`,
%% which is exactly the shape that must suspend the *calling* process in-machine
%% rather than folding into the uncatchable `blocked()` gap at the call site.
%%
%% `blocker/0` is that blocking callee: it waits for a `{reply, V}` message and
%% then answers `V`. Its oracle transcript is
%%
%%     erl -noshell -eval '
%%         Self = self(),
%%         P = spawn(fun() ->
%%                 Self ! {done, applyrecv:run(applyrecv, blocker, [])} end),
%%         P ! {reply, 42},
%%         receive {done, V} -> io:format("oracle=~p~n", [V]) end, halt().'
%%
%% which prints `oracle=42` (OTP 29 / erts-17.1).
-module(applyrecv).
-export([run/3, blocker/0]).

%% `apply(M, F, A)` with runtime `M`/`F`/`A`: `call_ext_only 3 erlang:apply/3`.
run(M, F, A) ->
    erlang:apply(M, F, A).

%% The blocking callee a `run(applyrecv, blocker, [])` targets.
blocker() ->
    receive
        {reply, V} -> V
    end.
