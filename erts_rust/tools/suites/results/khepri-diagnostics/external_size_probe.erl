%% Diagnostic probe for RC-C of `results/khepri-notes.md`.
%%
%% `khepri_machine:compute_command_size/1` calls `erlang:external_size/1` on the
%% command term, which can carry a stored procedure (`#Fun<...>`). This runtime
%% cannot encode a local fun into ETF and raises `error:unsupported_local_fun`,
%% so `display_tree:machine_overview_test/0` fails; the oracle answers a size.
%%
%% Erlc: `erlc -o . external_size_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'external_size_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'external_size_probe:main(), halt(0).'`
-module(external_size_probe).
-export([main/0]).

main() ->
    Fun = fun() -> ok end,
    Named = fun Fact(0) -> 1; Fact(N) -> N * Fact(N - 1) end,
    show("erlang:external_size(fun() -> ok end)",
         attempted(fun() -> erlang:external_size(Fun) end)),
    show("erlang:external_size({x, fun() -> ok end, y})",
         attempted(fun() -> erlang:external_size({x, Fun, y}) end)),
    show("term_to_binary({x, fun() -> ok end, y})",
         attempted(fun() -> byte_size(term_to_binary({x, Fun, y})) end)),
    show("term_to_binary(named fun)",
         attempted(fun() -> byte_size(term_to_binary(Named)) end)),
    %% The nested shapes: the encoder's local-fun identity is installed for the
    %% whole term only when the top-level term *is* the local fun, so these are
    %% the discriminating cases.
    show("term_to_binary({fun() -> ok end})",
         attempted(fun() -> byte_size(term_to_binary({Fun})) end)),
    show("term_to_binary({x, fun() -> ok end})",
         attempted(fun() -> byte_size(term_to_binary({x, Fun})) end)),
    show("term_to_binary([fun() -> ok end])",
         attempted(fun() -> byte_size(term_to_binary([Fun])) end)),
    show("term_to_binary(#{k => fun() -> ok end})",
         attempted(fun() -> byte_size(term_to_binary(#{k => Fun})) end)),
    show("erlang:external_size([fun() -> ok end])",
         attempted(fun() -> erlang:external_size([Fun]) end)),
    show("term_to_binary(an atom)   %% control",
         attempted(fun() -> byte_size(term_to_binary(an_atom)) end)),
    ok.

attempted(Fun) ->
    try Fun() of
        Value -> {ok, Value}
    catch
        Class:Reason -> {Class, Reason}
    end.

show(Name, Value) ->
    io:format("~-46s => ~p~n", [Name, Value]).
