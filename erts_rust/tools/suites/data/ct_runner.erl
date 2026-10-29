%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2025-2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% A minimal `common_test`-case driver, used by `tools/suites` to run OTP's stdlib suites
%% under *both* the system ERTS and the Rust runtime.
%%
%% `ct` itself needs a node, a test directory layout and config files; this runner needs
%% none of that: it expands `Suite:all()` (recursively through `groups/0`), runs each case in
%% its own monitored process with a timeout, and prints one canonical line per case so the
%% driver can compare the two sides case by case:
%%
%%     CASE <name> OK | SKIP <reason> | EXC <reason> | TIMEOUT | MISSING
%%     SUITE_INIT_FAILED <reason> | SUITE_SKIP <reason> | SUITE_LOAD_FAILED <reason>
%%     SUITE <name>
%%     DONE <name>
%%
%% Deliberately NOT implemented: `init_per_group`/`end_per_group`/`init_per_testcase`/
%% `end_per_testcase` (and `{init_per_group, …}` group properties). A case that depends on
%% them fails the same way on both sides, which the driver reports as `agree-fail-identical`
%% and excludes from both numerators rather than hiding.
-module(ct_runner).

-export([run/3, cases/1]).

%% @doc Runs every case of `Suite' and prints the canonical per-case lines.
-spec run(atom(), non_neg_integer(), file:filename()) -> ok.
run(Suite, TimeoutMs, DataRoot) ->
    io:format("SUITE ~p~n", [Suite]),
    DataDir = data_dir(Suite, DataRoot),
    Base = [{suite, Suite},
            {data_dir, DataDir},
            {priv_dir, DataDir},
            {case_timeout, TimeoutMs}],
    case code:ensure_loaded(Suite) of
        {module, Suite} ->
            case init_per_suite(Suite, Base) of
                {runner_skip, SkipReason} ->
                    io:format("SUITE_SKIP ~s~n", [fmt(SkipReason)]);
                {runner_init_failed, InitReason} ->
                    io:format("SUITE_INIT_FAILED ~s~n", [fmt(InitReason)]);
                Config ->
                    lists:foreach(fun(Case) -> run_case(Suite, Case, Config, TimeoutMs) end,
                                  cases(Suite)),
                    end_per_suite(Suite, Config)
            end;
        LoadError ->
            io:format("SUITE_LOAD_FAILED ~s~n", [fmt(LoadError)])
    end,
    io:format("DONE ~p~n", [Suite]),
    ok.

%% @doc The expanded case list of a suite: `all/0' with `groups/0' flattened, sorted and
%% deduplicated so the order is stable across runs and across runtimes.
-spec cases(atom()) -> [atom()].
cases(Suite) ->
    Groups = try Suite:groups() catch _:_ -> [] end,
    All = case erlang:function_exported(Suite, all, 0) of
              true ->
                  try Suite:all() catch _:_ -> [] end;
              false ->
                  %% A suite without `all/0' (not a common_test suite at all): every exported
                  %% nullary function that is not a `ct' callback is a candidate case.
                  [F || {F, 0} <- Suite:module_info(exports),
                        not lists:member(F, [module_info, all, groups, init_per_suite,
                                             end_per_suite, init_per_group, end_per_group,
                                             init_per_testcase, end_per_testcase])]
          end,
    expand(All, Groups, []).

expand(Items, Groups, Seen) ->
    lists:usort(lists:flatmap(fun(Item) -> expand_item(Item, Groups, Seen) end, Items)).

expand_item(Case, _Groups, _Seen) when is_atom(Case) ->
    [Case];
expand_item({group, Group}, Groups, Seen) ->
    expand_group(Group, Groups, Seen);
expand_item({group, Group, _Props}, Groups, Seen) ->
    expand_group(Group, Groups, Seen);
expand_item({group, Group, _Props, Sub}, Groups, Seen) ->
    expand_sub(Group, Sub, Groups, Seen);
expand_item(_Other, _Groups, _Seen) ->
    [].

expand_group(Group, Groups, Seen) ->
    case lists:keyfind(Group, 1, Groups) of
        {Group, _Props, Sub} -> expand_sub(Group, Sub, Groups, Seen);
        _ -> [Group]
    end.

expand_sub(Group, Sub, Groups, Seen) ->
    case lists:member(Group, Seen) of
        true -> [];
        false -> expand(Sub, Groups, [Group | Seen])
    end.

data_dir(Suite, DataRoot) ->
    Candidate = filename:join(DataRoot, atom_to_list(Suite) ++ "_data"),
    case filelib:is_dir(Candidate) of
        true -> Candidate;
        false -> DataRoot
    end.

init_per_suite(Suite, Base) ->
    case erlang:function_exported(Suite, init_per_suite, 1) of
        false ->
            Base;
        true ->
            try Suite:init_per_suite(Base) of
                {skip, Reason} -> {runner_skip, Reason};
                {skip_and_save, Reason, _} -> {runner_skip, Reason};
                {fail, Reason} -> {runner_init_failed, Reason};
                Config when is_list(Config) -> Config;
                Other -> {runner_init_failed, {unexpected_return, Other}}
            catch
                Class:Reason:Stack -> {runner_init_failed, {Class, Reason, first_frame(Stack)}}
            end
    end.

end_per_suite(Suite, Config) ->
    case erlang:function_exported(Suite, end_per_suite, 1) of
        false ->
            ok;
        true ->
            try Suite:end_per_suite(Config) catch _:_ -> ok end
    end.

run_case(Suite, Case, Config, TimeoutMs) ->
    Callable = erlang:function_exported(Suite, Case, 0)
        orelse erlang:function_exported(Suite, Case, 1),
    case Callable of
        false ->
            io:format("CASE ~p MISSING~n", [Case]);
        true ->
            {Pid, Ref} = spawn_monitor(
                           fun() -> exit({ct_case_result, call_case(Suite, Case, Config)}) end),
            receive
                {'DOWN', Ref, process, Pid, {ct_case_result, ok}} ->
                    io:format("CASE ~p OK~n", [Case]);
                {'DOWN', Ref, process, Pid, {ct_case_result, {skip, Reason}}} ->
                    io:format("CASE ~p SKIP ~s~n", [Case, fmt(Reason)]);
                {'DOWN', Ref, process, Pid, Reason} ->
                    io:format("CASE ~p EXC ~s~n", [Case, fmt(Reason)])
            after TimeoutMs ->
                exit(Pid, kill),
                receive {'DOWN', Ref, process, Pid, _} -> ok after 5000 -> ok end,
                io:format("CASE ~p TIMEOUT~n", [Case])
            end
    end.

call_case(Suite, Case, Config) ->
    Result = try
                 case erlang:function_exported(Suite, Case, 1) of
                     true -> Suite:Case(Config);
                     false -> Suite:Case()
                 end
             catch
                 Class:Reason:Stack -> {case_exception, Class, Reason, first_frame(Stack)}
             end,
    normalize(Result).

normalize({case_exception, Class, Reason, Frame}) ->
    exit({exception, Class, Reason, Frame});
normalize({skip, Reason}) ->
    {skip, Reason};
normalize({skip_and_save, Reason, _}) ->
    {skip, Reason};
normalize({fail, Reason}) ->
    exit({failed, Reason});
normalize({error, Reason}) ->
    exit({returned_error, Reason});
normalize({comment, _}) ->
    ok;
normalize({save_config, _}) ->
    ok;
normalize(_Other) ->
    ok.

first_frame([Frame | _]) -> Frame;
first_frame(_) -> none.

%% A single-line, depth-limited rendering: the driver compares statuses, and the text is
%% only there to make a divergence diagnosable. Newlines would break the line protocol.
fmt(Term) ->
    Flat = try lists:flatten(io_lib:format("~0P", [Term, 8]))
           catch _:_ -> lists:flatten(io_lib:format("~p", [Term]))
           end,
    OneLine = [case C of
                   $\n -> $\s;
                   $\r -> $\s;
                   $\t -> $\s;
                   _ -> C
               end || C <- Flat],
    truncate(OneLine, 300).

truncate(Text, Max) when length(Text) =< Max ->
    Text;
truncate(Text, Max) ->
    lists:sublist(Text, Max) ++ "...".
