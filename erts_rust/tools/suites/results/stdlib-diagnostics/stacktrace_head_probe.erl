%% Diagnostic probe for class D5 of `results/stdlib-notes.md` §D.
%%
%% Old observation (`maps_SUITE:t_from_keys`, `t_with_2`, `t_without_2`): the
%% stacktrace head of a raising BIF named the *caller* instead of the BIF —
%% `{mm,with2,0,[{file,…}]}` where the oracle has `{maps,with,[[a,b],a],…}`.
%%
%% Measured from a **compiled** module on purpose: the `-eval` tree-walk
%% evaluator reports an empty stacktrace for every raise on the rust side, which
%% would hide the frame question behind a different artefact.
%%
%% Erlc: `erlc -o . stacktrace_head_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'stacktrace_head_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'stacktrace_head_probe:main(), halt(0).'`
-module(stacktrace_head_probe).
-export([main/0]).

main() ->
    row("maps:with/2 (notes shape)", fun() -> maps:with([a, b], id(a)) end),
    row("maps:keys/1", fun() -> maps:keys(a) end),
    row("maps:get/2", fun() -> maps:get(a, b) end),
    row("lists:reverse/1", fun() -> lists:reverse(a) end),
    row("binary:part/3", fun() -> binary:part(<<1, 2>>, 0, 5) end),
    ok.

id(X) -> X.

row(Label, F) ->
    Trace = try F() of
                V -> {returned, V}
            catch
                C:R:S -> {C, R, S}
            end,
    io:format("~-28s : ~p~n", [Label, short(Trace)]).

%% Print only the frame heads: a whole trace from the oracle is long, and the
%% question is only *which* frame comes first.
short({C, R, S}) -> {C, short_reason(R), [head(F) || F <- lists:sublist(S, 3)]};
short(Other) -> Other.

short_reason({badmap, M}) -> {badmap, M};
short_reason({badarg, _}) -> badarg;
short_reason(R) -> R.

head({M, F, A, _Loc}) -> {M, F, A};
head(F) -> F.
