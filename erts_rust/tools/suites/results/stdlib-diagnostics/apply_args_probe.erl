%% Diagnostic probe for class D9 of `results/stdlib-notes.md` §D.
%%
%% The shape in `queue_SUITE` is `call/2` (`lib/stdlib/test/queue_SUITE.erl`):
%%
%%     call(Func, As) ->
%%         try case Func of
%%                 {M,F} -> apply(M, F, As);
%%                 _     -> apply(Func, As)
%%             end of ...
%%
%% This probe sweeps both argument positions; every row is fully guarded so one
%% failing shape never aborts the run.
%%
%% MECHANISM CORRECTED (class D11 — see `FINDINGS.md` §*queue_SUITE:oops — the value
%% disagreement* and the `queue_oops_probe.erl` probe): the rows that pass here do NOT pass
%% because of argument position. `erlang:apply/2` is unimplemented in this runtime: a genuine
%% `call_ext erlang:apply/2` reaches the `beam_rt` native floor, which reads `args[0]` as a
%% module and `args[1]` as a function and calls `M:F()`. What separates pass from fail is
%% whether the **compiler** rewrote the call: when the argument list's length is statically
%% known the call becomes `call_fun` (`erlc -S` proves it on `only_fun/1`), so it never
%% reaches the floor; when the length is unknown it stays `erlang:apply/2` and raises
%% `badarg`. So "fun param, literal args" passes and "literal fun, args param" fails for the
%% length, not for the position — and `apply(erlang, self)` (two atoms) *runs* `erlang:self/0`
%% where the oracle raises `badarg`.
%%
%% Erlc: `erlc -o . apply_args_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'apply_args_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'apply_args_probe:main(), halt(0).'`
-module(apply_args_probe).
-export([main/0, smallest/1, only_fun/1, only_list/1, both/2]).

-define(F, fun(X) -> X end).

main() ->
    Lit = [{"lbl", ?F, [a]}],
    report("smallest/1 (notes shape)", fun() -> smallest([a]) end),
    report("local fun, literal args", fun() -> local() end),
    report("fun param, literal args", fun() -> only_fun(?F) end),
    report("literal fun, args param", fun() -> only_list([a]) end),
    report("both params", fun() -> both(?F, [a]) end),
    report("param rebind, then apply/2", fun() -> param_indirect(?F, [a]) end),
    report("body match, then apply/2", fun() -> body_match() end),
    report("clause pattern, apply/2", fun() -> head({?F, [a]}) end),
    report("ext fun, literal args", fun() -> only_fun(fun lists:reverse/1) end),
    report("ext fun, args param", fun() -> arglist(fun lists:reverse/1, [b, c]) end),
    report("apply/3 (params)", fun() -> apply3(lists, reverse, [[1, 2, 3]]) end),
    report("direct call F(Args)", fun() -> call_direct(?F, [a]) end),
    report("lists:foreach over lit", fun() ->
              lists:foreach(fun({_L, F, As}) -> throw({applied, apply(F, As)}) end, Lit) end),
    report("arity mismatch reason", fun() -> smallest([]) end),
    ok.

%% --- the shapes -----------------------------------------------------------

%% The smallest failing shape: the argument list's LENGTH is unknown at compile time.
smallest(Args) ->
    apply(fun(X) -> X end, Args).

local() ->
    F = ?F,
    apply(F, [a]).

only_fun(F) ->
    apply(F, [a]).

only_list(As) ->
    apply(?F, As).

both(F, As) ->
    apply(F, As).

%% An extra rebinding of both operands makes it work again.
param_indirect(F, As) ->
    G = F,
    A = As,
    apply(G, A).

body_match() ->
    {F, As} = {?F, [a]},
    apply(F, As).

head({F, As}) ->
    apply(F, As).

arglist(F, As) ->
    apply(F, As).

apply3(M, F, As) ->
    apply(M, F, As).

call_direct(F, A) ->
    F(A).

%% --- harness -------------------------------------------------------------

report(Label, F) ->
    Result = try F() of
                 ok -> ok;
                 V -> {ok, V}
             catch
                 throw:T -> T;
                 C:R -> {caught, C, short(R)}
             end,
    io:format("~-32s : ~p~n", [Label, Result]).

%% `{badarity,{Fun,Args}}` embeds a fun whose module index differs between
%% runtimes; keep only the arity-bearing half.
short({badarity, {_Fun, Args}}) -> {badarity, Args};
short(R) -> R.
