%% Oracle fixture for the `ExecutionContext::atom_name` seam
%% (`beam_engine/tests/apply_atom_value.rs`).
%%
%% The callee an `atomapply:run(atomtarget, target_fun, [])` targets. Its module
%% name and export name never appear in `atomapply`'s source, so a test can intern
%% them into the **live** shared table *after* loading `atomapply` and hand their
%% global atom ids — beyond `atomapply`'s own load-time snapshot — to the apply.
%%
%% Oracle transcript (OTP 29 / erts-17.1):
%%
%%     erl -noshell -eval 'io:format("oracle=~p~n",
%%         [atomapply:run(atomtarget, target_fun, [])]), halt().'
%%
%% which prints `oracle=42`.
-module(atomtarget).
-export([target_fun/0]).

target_fun() ->
    42.
