%% Minimal eunit fixture module for `block_wall_probe:eunit_here/1`: an eunit
%% run started from the `-eval` evaluator drives `eunit_proc` on a **nested**
%% machine, so this module isolates how much of the Khepri fixture is needed
%% before that nested run stops advancing.
%%
%% Run:  `beam_rt -noshell -pa <beams> -eval 'block_wall_probe:eunit_here(block_wall_fixture), halt(0).'`
%%       `erl     -noshell -pa <beams> -eval 'block_wall_probe:eunit_here(block_wall_fixture), halt(0).'`
-module(block_wall_fixture).
-include_lib("eunit/include/eunit.hrl").
-export([fixtures/1, child/1]).

-define(FIXTURES, 30).

fixtures(N) ->
    [{lists:flatten(io_lib:format("fixture_~p", [I])),
      {setup,
       fun() -> ok end,
       fun(_) -> ok end,
       fun(_) ->
               [?_assertEqual(I, child(I))]
       end}}
     || I <- lists:seq(1, N)].

child(I) ->
    Parent = self(),
    Pid = spawn(fun() -> Parent ! {self(), I * 1} end),
    receive {Pid, V} -> V after 60000 -> timeout end.

all_test_() ->
    fixtures(?FIXTURES).
