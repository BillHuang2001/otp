%% Khepri-fixture eunit module for `block_wall_probe:eunit_here/1`: the same
%% `test_ra_server_helpers:setup/1` + `cleanup/1` fixture the blocked rows use,
%% but written as `N` tiny independent fixtures, so the nested-run wall can be
%% reproduced without a whole Khepri test module.
%%
%% Run:  `beam_rt -noshell <khepri -pa…> -pa <beams> -eval 'block_wall_probe:eunit_here(block_wall_khepri), halt(0).'`
%%       `beam_rt -noshell <khepri -pa…> -pa <beams> -eval 'block_wall_probe:eunit_spawned(block_wall_khepri), halt(0).'`
%% (run each side in its own throwaway cwd: every fixture creates a data dir)
-module(block_wall_khepri).
-include_lib("eunit/include/eunit.hrl").
-export([fixtures/1]).

-define(FIXTURES, 20).

fixtures(N) ->
    [fixture(I) || I <- lists:seq(1, N)].

fixture(I) ->
    Name = list_to_atom("block_wall_khepri_" ++ integer_to_list(I)),
    {lists:flatten(io_lib:format("fixture_~p", [I])),
     {setup,
      fun() -> ck(I, "setup start"),
               Props = test_ra_server_helpers:setup(Name),
               ck(I, "setup done"),
               Props
      end,
      fun(Props) -> ck(I, "cleanup start"),
                    R = test_ra_server_helpers:cleanup(Props),
                    ck(I, "cleanup done"),
                    R
      end,
      fun(Props) ->
              StoreId = maps:get(store_id, Props),
              ck(I, "body"),
              [?_test(begin
                          ck(I, "create"),
                          ?assertEqual(ok, khepri:create(StoreId, [foo], I)),
                          ck(I, "create ok")
                      end),
               ?_test(begin
                          ck(I, "get"),
                          ?assertEqual({ok, I}, khepri:get(StoreId, [foo])),
                          ck(I, "get ok")
                      end),
               ?_test(begin
                          ck(I, "count"),
                          ?assertEqual({ok, 1}, khepri:count(StoreId, [foo])),
                          ck(I, "count ok")
                      end)]
      end}}.

%% Unbuffered checkpoint: stdout through the io device is buffered, stderr is
%% not, so this survives a run the runtime aborts mid-fixture.
ck(I, What) ->
    io:format(standard_error, "FIX ~p ~s~n", [I, What]).

all_test_() ->
    fixtures(?FIXTURES).
