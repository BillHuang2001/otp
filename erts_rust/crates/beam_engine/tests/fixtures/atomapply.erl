%% Oracle fixture for the `ExecutionContext::atom_name` seam
%% (`beam_engine/tests/apply_atom_value.rs`).
%%
%% `run/3` applies a **runtime atom value**: because `M`/`F`/`A` are parameters
%% the compiler cannot fold them into a direct call, so the body lowers to
%% `call_ext_only 3 {extfunc,erlang,apply,3}` (verified with
%% `beam_disasm:file/1`). The engine special-cases that import in
%% `Machine::resolve_call_ext_apply` and funnels it into
%% `families::funs::apply_target`, which must resolve the runtime `M`/`F` atoms.
%%
%% This is the shape of the A.9 boot-path bug: at `gen_server:init_it/6` the
%% engine evaluates `{ok, Mod:init(Args)}` with `Mod = supervisor_bridge`, an
%% atom interned into the shared table *after* `gen_server` was loaded — so it is
%% beyond `gen_server`'s owned load-time atom snapshot and a bare module-local
%% lookup cannot name it. The target fixture `atomtarget` is a module whose name
%% and function are absent from this module's own source, i.e. unnameable through
%% this module's snapshot.
%%
%% Oracle transcript (OTP 29 / erts-17.1):
%%
%%     erl -noshell -eval 'io:format("oracle=~p~n",
%%         [atomapply:run(atomtarget, target_fun, [])]), halt().'
%%
%% which prints `oracle=42`.
-module(atomapply).
-export([run/3]).

%% `apply(M, F, A)` with runtime `M`/`F`/`A`: `call_ext_only 3 erlang:apply/3`.
run(M, F, A) ->
    erlang:apply(M, F, A).
