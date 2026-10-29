%% Diagnostic probe for RC-D of `results/khepri-notes.md` — the keep-while
%% delete fan-out that `keep_while_conditions` asserts must finish in under
%% one second (`?assert(DeletionTime < 1000)` in
%% `keep_while_conditions:delete_node_with_many_dependents/0`).
%%
%% The module cannot be run through eunit on the Rust runtime because its
%% *other* rows start an `ra` system (RC-A/RC-B), but the two performance
%% tests themselves are pure `khepri_machine` state-machine work: they build
%% state with `khepri_machine:init/1` + `khepri_machine:apply/3` and time a
%% single `#delete{}`. This probe reproduces both loops standalone, so the
%% measurement no longer depends on the `ra` fixture.
%%
%% It prints *counts* (deterministic, comparable) and *milliseconds*
%% (host-specific, never portable — only the same-host ratio is meaningful).
%%
%% Compile (the staged Khepri build supplies the headers and the beams):
%%   PROJ=/tmp/erts-suites-stage/khepri/proj
%%   erlc -DTEST -I "$PROJ" -o <out> keep_while_fanout_probe.erl
%% Run (one throwaway cwd per side; the -pa list is every
%% `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test`):
%%   beam_rt -noshell <pa...> -eval 'keep_while_fanout_probe:main(), halt(0).'
%%   erl     -noshell <pa...> -eval 'keep_while_fanout_probe:main(), halt(0).'
-module(keep_while_fanout_probe).
-include("include/khepri.hrl").
-include("src/khepri_machine.hrl").
-include("test/helpers.hrl").
-export([main/0, scaling/0]).

%% `keep_while_conditions:delete_node_with_many_dependents/0`: one target node
%% with `?MAX_SIBLINGS_PER_PATTERN * 5 div 2` keep-while dependents.
fanout_dependents() -> 2500.
%% `keep_while_conditions:delete_node_with_dependency_chain/0`: a chain in
%% which each node depends on the previous one.
chain_length() -> 100.

main() ->
    fanout(fanout_dependents()),
    chain(),
    ok.

%% Same-host scaling sweep: the delete of one target node with `N` keep-while
%% dependents, for several `N`. `delete_us_per_dependent` is what separates a
%% constant-factor slowdown (flat across `N`) from a super-linear one (rising).
scaling() ->
    lists:foreach(fun(N) -> fanout(N) end, [250, 500, 1000, 2500]),
    ok.

%% ---------------------------------------------------------------- fan-out
fanout(N) ->
    S0 = khepri_machine:init(#{store_id => fanout,
                               member => khepri_cluster:this_member(fanout),
                               commands => [#put{path = [target],
                                                 payload =
                                                     khepri_payload:data(value)}]}),
    KeepWhile = #{[target] => #if_node_exists{exists = true}},
    Paths = [[dependent, integer_to_binary(I)] || I <- lists:seq(1, N)],
    T0 = erlang:monotonic_time(millisecond),
    S1 = lists:foldl(fun(Path, SAcc) ->
                             Command = #put{path = Path,
                                            payload = khepri_payload:none(),
                                            options = #{keep_while => KeepWhile}},
                             {SNext, {ok, _}, _SE} =
                                 khepri_machine:apply(?META, Command, SAcc),
                             SNext
                     end, S0, Paths),
    T1 = erlang:monotonic_time(millisecond),
    {_S2, Ret, _SE} = khepri_machine:apply(?META,
                                           #delete{path = [target]},
                                           S1),
    T2 = erlang:monotonic_time(millisecond),
    Deleted = case Ret of
                  {ok, M} -> M;
                  Other -> Other
              end,
    io:format("FANOUT dependents=~p insert_ms=~p delete_ms=~p "
              "delete_us_per_dependent=~p deleted_count=~p under_1000=~p~n",
              [N, T1 - T0, T2 - T1,
               (T2 - T1) * 1000 div N,
               case is_map(Deleted) of true -> maps:size(Deleted); false -> Deleted end,
               (T2 - T1) < 1000]).

%% ------------------------------------------------------------------ chain
chain() ->
    N = chain_length(),
    S0 = khepri_machine:init(#{store_id => chain,
                               member => khepri_cluster:this_member(chain),
                               commands =>
                                   [#put{path = [<<"1">>],
                                         payload = khepri_payload:none()}]}),
    T0 = erlang:monotonic_time(millisecond),
    S1 = lists:foldl(fun(I, SAcc) ->
                             ThisNode = integer_to_binary(I),
                             PrevNode = integer_to_binary(I - 1),
                             KeepWhile =
                                 #{[PrevNode] => #if_node_exists{exists = true}},
                             Command = #put{path = [ThisNode],
                                            payload = khepri_payload:none(),
                                            options = #{keep_while => KeepWhile}},
                             {SNext, {ok, _}, _SE} =
                                 khepri_machine:apply(?META, Command, SAcc),
                             SNext
                     end, S0, lists:seq(2, N)),
    T1 = erlang:monotonic_time(millisecond),
    {_S2, Ret, _SE} = khepri_machine:apply(?META,
                                           #delete{path = [<<"1">>]},
                                           S1),
    T2 = erlang:monotonic_time(millisecond),
    Deleted = case Ret of
                  {ok, M} -> M;
                  Other -> Other
              end,
    io:format("CHAIN length=~p insert_ms=~p delete_ms=~p "
              "deleted_count=~p under_1000=~p~n",
              [N, T1 - T0, T2 - T1,
               case is_map(Deleted) of true -> maps:size(Deleted); false -> Deleted end,
               (T2 - T1) < 1000]).
