%%
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2026. All Rights Reserved.
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
%% stage2_suite_driver -- run the Stage-2 compiler-pass Common Test suites
%% without ct_run / ct:run_test.
%%
%% WHY THIS EXISTS
%%   `suite/0' of every suite in scope returns `{ct_hooks,[ts_install_cth]}',
%%   and `ts_install_cth' is *not* installed by a plain in-tree build, so
%%   neither `ct_run' nor `ct:run_test/1' can start these suites (see
%%   lib/compiler/test/CONTEXT.md, "Running A Single Suite Without ct_run").
%%   This module therefore *is* the runner: it loads a suite, calls the
%%   Common Test callbacks itself and catches every test-case failure itself.
%%
%%   It is driven by `run_stage2_suites.sh' in this directory, which builds a
%%   rig compiler for the host (`lib/compiler/scripts/host_env.sh build'),
%%   compiles the suites with that compiler into a private scratch ebin and
%%   then runs
%%
%%       erl -noshell -pa "$RIG_EBIN" -pa "$SCRATCH/ebin" \
%%           -run stage2_suite_driver main "$REPORT" SUITE...
%%
%%   `-run' passes every command-line argument as a flat string list, so
%%   `main/1' receives `[Report, Suite | _]'.  Without arguments it runs the
%%   five suites listed in ?DEFAULT_SUITES and writes the default report.
%%
%% WHAT IT DOES
%%   * `init_per_suite([{priv_dir,Dir}])' -- Dir is created if needed and is
%%     either `$STAGE2_PRIV_BASE/<suite>_priv' or a fresh directory under
%%     `$TMPDIR'.  The suites write their probe modules there.
%%   * walks `all/0'; a testcase is called with the Config, a
%%     `{group,G}' / `{group,G,Props}' entry runs `init_per_group(G,Config)',
%%     the testcases that `groups/0' lists for G (2- and 3-tuple group
%%     definitions; non-testcase entries such as the `parallel' property are
%%     skipped) and finally `end_per_group(G,Config)'.
%%   * `end_per_suite(Config)'.
%%   * every callback call is wrapped in `try ... catch Class:Reason:Stack';
%%     `ct:fail/1' is just `exit({test_case_failed,Reason})' and is rendered
%%     as such.
%%
%% NATIVE RECORDS
%%   A suite that includes `beam_ssa.hrl'/`beam_types.hrl' (only
%%   `beam_ssa_loop_opt_SUITE' among the Stage-2 suites) is compiled by the rig
%%   compiler into the OTP-30-only `update_record_id' instruction, which the
%%   OTP-29 host emulator refuses to load.  Such a suite is transparently
%%   recompiled with `no_native_record_opt' and loaded from memory; a `NOTE'
%%   line says so.  The test cases that build a native record then fail with
%%   `error:{badrecord,...}' -- a host limitation, never a suite failure -- and
%%   the ones that only compile and run probe modules still give a real result.%%
%% RESULTS
%%   One `PASS' / `FAIL' / `SKIP' / `WARN' line per testcase, a per-suite and a
%%   final summary, all of it printed on stdout *and* accumulated into the
%%   report file: `halt/1' does not flush the io queue, so the report is
%%   written with file:write_file/2 before halting, and
%%   `erlang:halt(Status,[{flush,true}])' is used.
%%
%%   Status is 1 if any testcase failed, else 0.  `WARN' marks a testcase that
%%   returned neither `ok' nor a `{comment,_}'/`{skip,_}'/`{error,_}' tuple
%%   (Common Test would treat that as an error, but the return value is a
%%   runner concern and never a compilation failure); it does not affect the
%%   exit status.
%%
%% DELIBERATE LIMITATIONS
%%   * Test cases run sequentially: the `parallel' group property is ignored
%%     (it only affects scheduling, never results).
%%   * No timetrap is enforced (`suite/0's `{timetrap,...}' is a Common Test
%%     feature).
%%   * No log files, no `ct:pal' capture: `ct:pal/2' still works (it warns
%%     "ct_logs not started") and writes to the driver's stdout.
%%

-module(stage2_suite_driver).

-export([main/1]).

%% The Stage-2 compiler-pass suites this directory provides.
-define(DEFAULT_SUITES,
        [beam_ssa_loop_opt_SUITE,
         beam_ssa_fus_SUITE,
         beam_ssa_fbip_SUITE,
         ssa_opt_budget_SUITE,
         beam_ssa_opt_equiv_SUITE]).

%% Longest failure reason (and stack) rendered per line, in characters.
-define(MAX_DETAIL, 8000).

%% Accumulated output and the per-status counters.
-record(st, {lines = [] :: [iodata()],
             pass = 0 :: non_neg_integer(),
             fail = 0 :: non_neg_integer(),
             skip = 0 :: non_neg_integer(),
             warn = 0 :: non_neg_integer()}).

%%--------------------------------------------------------------------------
%% Entry point
%%--------------------------------------------------------------------------

-spec main([string()]) -> no_return().
main(Args) ->
    {Report, Suites} = parse_args(Args),
    St = run_suites(Suites),
    write_report(Report, lists:reverse(St#st.lines)),
    erlang:halt(final_status(St), [{flush, true}]).

parse_args([]) ->
    {default_report(), ?DEFAULT_SUITES};
parse_args([Report | Suites]) ->
    Named = [list_to_atom(S) || S <- Suites, S =/= ""],
    {Report, case Named of
                 [] -> ?DEFAULT_SUITES;
                 _ -> Named
             end}.

default_report() ->
    "stage2_suites_report.txt".

%%--------------------------------------------------------------------------
%% Driving the suites
%%--------------------------------------------------------------------------

run_suites(Suites) ->
    St0 = emit(#st{}, ["",
                       "==== Stage-2 compiler-pass Common Test suites ====",
                       "compiler used: " ++ fmt(code:which(compile)),
                       "suites: " ++
                           string:join([atom_to_list(S) || S <- Suites], ", ")]),
    do_suites(Suites, St0).

do_suites([], St) ->
    final_summary(St);
do_suites([Suite | T], St0) ->
    do_suites(T, run_one_suite(Suite, St0)).

%% Run one suite and print its per-suite summary. A crash in the *runner*
%% (not in a test case -- those are caught by call/3) is recorded as a failed
%% pseudo-testcase so that the remaining suites still run.
run_one_suite(Suite, St0) ->
    St1 = emit(St0, ["", "=== SUITE " ++ atom_to_list(Suite) ++ " ==="]),
    Counts0 = counts(St1),
    St2 = try
              do_suite(Suite, St1)
          catch
              Class:Reason:Stack ->
                  record(St1, fail, Suite, '<driver crashed>',
                         {exception, Class, Reason, Stack})
          end,
    {Pass, Fail, Skip, Warn} = sub(counts(St2), Counts0),
    emit(St2, [io_lib:format("--- ~s: ~w pass, ~w fail, ~w skip, ~w warn ---",
                             [Suite, Pass, Fail, Skip, Warn])]).

%% Load the suite, then run init_per_suite / all/0 / end_per_suite.
do_suite(Suite, St0) ->
    Priv = priv_dir(Suite),
    case load_or_rescue(Suite) of
        {ok, []} ->
            do_suite_1(Suite, Priv, St0);
        {ok, Note} ->
            do_suite_1(Suite, Priv, emit(St0, Note));
        {error, Reason} ->
            record(St0, fail, Suite, '<load>', Reason)
    end.

do_suite_1(Suite, Priv, St0) ->
    Config0 = [{priv_dir, Priv}],
    case call(Suite, init_per_suite, [Config0]) of
        {ok, Config} when is_list(Config) ->
            St1 = walk_all(Suite, Config, St0),
            case call(Suite, end_per_suite, [Config]) of
                {ok, _} ->
                    St1;
                Fail ->
                    record(St1, fail, Suite, 'end_per_suite', Fail)
            end;
        {ok, {skip, Reason}} ->
            record(St0, skip, Suite, 'init_per_suite', {skip, Reason});
        Fail ->
            record(St0, fail, Suite, 'init_per_suite', Fail)
    end.

load(Suite) ->
    %% Trap: code:ensure_loaded/1 returns {module,M} -- not {ok,M} -- when the
    %% module is already loaded.
    case code:ensure_loaded(Suite) of
        {module, Suite} -> ok;
        {ok, Suite} -> ok;
        Other -> {error, Other}
    end.

%% Load a suite, falling back to a `no_native_record_opt' recompile when the
%% beam the rig compiler produced cannot be loaded by the *host* runtime.
%%
%% WHY THE FALLBACK IS NEEDED: the SSA records are OTP-30 *native records*
%% (`beam_ssa.hrl' and `beam_types.hrl' are `-import_record' headers), so the
%% rig compiler emits the OTP-30-only `update_record_id' instruction, which the
%% OTP-29 host emulator refuses to load at all ("This BEAM file was compiled
%% for a later version of the runtime system ... Use of opcode 193; this
%% emulator supports only up to 191").  With `no_native_record_opt' the suite
%% is loadable and only the test cases that actually *build* a #b_set{}/
%% #b_literal{}/#t_integer{} value fail -- with a precise
%% `error:{badrecord,{beam_ssa,b_literal}}', because the host runtime has no
%% native-record term type -- instead of the whole suite being unrunnable.
%% The fallback is reported in the output so such a failure is never mistaken
%% for a suite failure.
load_or_rescue(Suite) ->
    case load(Suite) of
        ok ->
            {ok, []};
        {error, Reason} ->
            case native_record_rescue(Suite) of
                ok ->
                    {ok, ["NOTE " ++ atom_to_list(Suite) ++
                          ": the rig-compiled beam cannot be loaded by the"
                          " host runtime (" ++ fmt(Reason) ++ ")",
                          "     recompiled it with +no_native_record_opt and"
                          " loaded that instead; test cases that build native",
                          "     records (#b_*{}/#t_*{}) then fail with"
                          " `badrecord' -- a host limitation, not a suite"
                          " failure."]};
                {error, Rescued} ->
                    {error, {load_failed, Reason, rescue_failed, Rescued}}
            end
    end.

%% Recompile the suite from source with the native-record optimisation off and
%% load the resulting binary. Uses the compiler that is first on the code path,
%% i.e. the rig compiler (the runner starts `erl' with `-pa $RIG_EBIN').
native_record_rescue(Suite) ->
    case suite_source(Suite) of
        {ok, Src} ->
            Opts = [binary, return_errors, return_warnings,
                    no_native_record_opt | inc_opts(filename:dirname(Src))],
            case compile:file(Src, Opts) of
                {ok, Suite, Bin} -> load_binary(Suite, Bin);
                {ok, Suite, Bin, _Warnings} -> load_binary(Suite, Bin);
                {ok, Other, _Bin, _Warnings} ->
                    {error, {wrong_module, Other}};
                {error, Errors, Warnings} ->
                    {error, {recompile_failed, Errors, Warnings}};
                Other ->
                    {error, {recompile_failed, Other}}
            end;
        {error, Reason} ->
            {error, Reason}
    end.

load_binary(Suite, Bin) ->
    case code:load_binary(Suite, atom_to_list(Suite) ++ ".erl", Bin) of
        {module, Suite} -> ok;
        Other -> {error, {load_binary_failed, Other}}
    end.

%% The suite's source file. Prefer the repo-root-relative path (the runner
%% starts erl in the repository root), so the recompiled module carries the
%% same `?FILE' as the other suites; `STAGE2_SUITE_SRC_DIR' -- the absolute
%% path of this directory, set by run_stage2_suites.sh -- is the fallback.
suite_source(Suite) ->
    Base = atom_to_list(Suite) ++ ".erl",
    Dir = case os:getenv("STAGE2_SUITE_SRC_DIR") of
              false -> "lib/compiler/test";
              "" -> "lib/compiler/test";
              D -> D
          end,
    Candidates = [filename:join("lib/compiler/test", Base),
                  filename:join(Dir, Base)],
    case [F || F <- Candidates, filelib:is_regular(F)] of
        [File | _] -> {ok, File};
        [] -> {error, {no_suite_source, Candidates}}
    end.

%% The include directories the suite sources need when they are recompiled
%% here: `-I lib/compiler/src' (beam_ssa.hrl, beam_types.hrl) and
%% `-I lib/stdlib/include', both derived from the test directory.
inc_opts(TestDir) ->
    [{i, filename:absname(filename:join([TestDir, "..", "src"]))},
     {i, filename:absname(filename:join([TestDir, "..", "..",
                                         "stdlib", "include"]))}].

%% Walk all/0: bare testcase atoms and {group,G} / {group,G,Props} entries.
walk_all(Suite, Config, St0) ->
    case call(Suite, all, []) of
        {ok, All} when is_list(All) ->
            walk_entries(Suite, All, Config, St0);
        Fail ->
            record(St0, fail, Suite, 'all/0', Fail)
    end.

walk_entries(_Suite, [], _Config, St) ->
    St;
walk_entries(Suite, [Entry | T], Config, St0) ->
    case Entry of
        {group, G} ->
            walk_entries(Suite, T, Config, walk_group(Suite, G, Config, St0));
        {group, G, _Props} ->
            walk_entries(Suite, T, Config, walk_group(Suite, G, Config, St0));
        TC when is_atom(TC) ->
            walk_entries(Suite, T, Config, run_case(Suite, TC, Config, St0));
        Other ->
            St1 = record(St0, fail, Suite, 'all/0',
                         {unexpected_entry, Other}),
            walk_entries(Suite, T, Config, St1)
    end.

%% init_per_group / testcases / end_per_group.
walk_group(Suite, G, Config, St0) ->
    case call(Suite, init_per_group, [G, Config]) of
        {ok, GroupConfig} when is_list(GroupConfig) ->
            St1 = case group_cases(Suite, G) of
                      {ok, Cases} ->
                          walk_cases(Suite, Cases, GroupConfig, St0);
                      undefined ->
                          record(St0, warn, Suite, {group, G},
                                 {group_not_in_groups_0, G})
                  end,
            case call(Suite, end_per_group, [G, GroupConfig]) of
                {ok, _} ->
                    St1;
                Fail ->
                    record(St1, fail, Suite, {end_per_group, G}, Fail)
            end;
        {ok, {skip, Reason}} ->
            record(St0, skip, Suite, {group, G}, {skip, Reason});
        Fail ->
            record(St0, fail, Suite, {group, G}, Fail)
    end.

%% The testcases of group G, taken from the *last* element of its groups/0
%% definition ({G,Cases} or {G,Props,Cases}).
group_cases(Suite, G) ->
    Groups = case call(Suite, groups, []) of
                 {ok, List} when is_list(List) -> List;
                 _ -> []
             end,
    case lists:keyfind(G, 1, Groups) of
        {G, _} = Def ->
            cases_of(lists:last(tuple_to_list(Def)));
        {G, _, _} = Def ->
            cases_of(lists:last(tuple_to_list(Def)));
        false ->
            undefined
    end.

cases_of(Cases) when is_list(Cases) -> {ok, Cases};
cases_of(_) -> {ok, []}.

walk_cases(_Suite, [], _Config, St) ->
    St;
walk_cases(Suite, [Case | T], Config, St0) ->
    St1 = case Case of
              {group, G} ->
                  walk_group(Suite, G, Config, St0);
              {group, G, _Props} ->
                  walk_group(Suite, G, Config, St0);
              TC when is_atom(TC) ->
                  case erlang:function_exported(Suite, TC, 1) of
                      true ->
                          run_case(Suite, TC, Config, St0);
                      false ->
                          %% A group property such as `parallel', not a
                          %% testcase.
                          record(St0, warn, Suite, TC, {not_a_testcase, TC})
                  end;
              Other ->
                  record(St0, warn, Suite, '<group entry>',
                         {unexpected_group_entry, Other})
          end,
    walk_cases(Suite, T, Config, St1).

%% Run a single testcase and classify its outcome.
run_case(Suite, TC, Config, St0) ->
    case call(Suite, TC, [Config]) of
        {ok, ok} ->
            record(St0, pass, Suite, TC, ok);
        {ok, {comment, Comment}} ->
            record(St0, pass, Suite, TC, {comment, Comment});
        {ok, {skip, Reason}} ->
            record(St0, skip, Suite, TC, {skip, Reason});
        {ok, {error, Reason}} ->
            record(St0, fail, Suite, TC, {error, Reason});
        {ok, {fail, Reason}} ->
            record(St0, fail, Suite, TC, {fail, Reason});
        {ok, Other} ->
            record(St0, warn, Suite, TC, {unexpected_return, Other});
        {exception, Class, Reason, Stack} ->
            record(St0, fail, Suite, TC, {exception, Class, Reason, Stack})
    end.

%% Call a suite callback, normalizing "not exported" to the value Common Test
%% would use (Config for init_*, ok for end_*), so that suites without
%% groups/0 or init_per_group/2 still work.
call(Suite, Fun, Args) ->
    case erlang:function_exported(Suite, Fun, length(Args)) of
        true ->
            try {ok, apply(Suite, Fun, Args)}
            catch
                Class:Reason:Stack -> {exception, Class, Reason, Stack}
            end;
        false ->
            {ok, missing_default(Fun, Args)}
    end.

missing_default(Fun, Args) ->
    case Fun of
        init_per_suite -> hd(Args);
        init_per_group -> lists:last(Args);
        end_per_group -> lists:last(Args);
        _ -> ok
    end.

%%--------------------------------------------------------------------------
%% Reporting
%%--------------------------------------------------------------------------

record(St0, Status, Suite, Case, Detail) ->
    Extra =
        case {Status, Detail} of
            {pass, {comment, Comment}} ->
                io_lib:format("  (~ts)", [Comment]);
            {pass, _} ->
                "";
            _ ->
                ": " ++ describe(Detail)
        end,
    Line = io_lib:format("~s ~s:~ts~ts",
                         [tag(Status), Suite, case_name(Case), Extra]),
    bump(emit(St0, [Line]), Status).

tag(pass) -> "PASS";
tag(fail) -> "FAIL";
tag(skip) -> "SKIP";
tag(warn) -> "WARN".

case_name(Case) when is_atom(Case) -> atom_to_list(Case);
case_name(Case) -> fmt(Case).

%% `ct:fail/1' is `exit({test_case_failed,Reason})'; render that the way Common
%% Test does, so a failure reason is recognizable at a glance.
describe({exception, exit, {test_case_failed, Reason}, _Stack}) ->
    io_lib:format("test_case_failed: ~ts", [fmt(Reason)]);
describe({exception, Class, Reason, Stack}) ->
    io_lib:format("~s:~ts~n      stack: ~ts",
                  [Class, fmt(Reason), fmt(lists:sublist(Stack, 8))]);
describe(Detail) ->
    fmt(Detail).

emit(#st{lines = Lines} = St, New) ->
    Io = [[L, "\n"] || L <- New],
    io:put_chars(Io),
    St#st{lines = lists:reverse(Io) ++ Lines}.

bump(#st{pass = P} = St, pass) -> St#st{pass = P + 1};
bump(#st{fail = F} = St, fail) -> St#st{fail = F + 1};
bump(#st{skip = S} = St, skip) -> St#st{skip = S + 1};
bump(#st{warn = W} = St, warn) -> St#st{warn = W + 1}.

counts(#st{pass = P, fail = F, skip = S, warn = W}) -> {P, F, S, W}.

sub({P1, F1, S1, W1}, {P0, F0, S0, W0}) ->
    {P1 - P0, F1 - F0, S1 - S0, W1 - W0}.

final_summary(St) ->
    {Pass, Fail, Skip, Warn} = counts(St),
    emit(St, ["",
              io_lib:format("==== TOTAL: ~w pass, ~w fail, ~w skip, ~w warn ====",
                            [Pass, Fail, Skip, Warn])]).

final_status(#st{fail = 0}) -> 0;
final_status(#st{}) -> 1.

write_report(Report, Text) ->
    Data = unicode:characters_to_binary(Text),
    case is_binary(Data) of
        true ->
            case file:write_file(Report, Data) of
                ok ->
                    io:format("~nreport written to ~ts~n", [Report]);
                {error, Reason} ->
                    io:format(standard_error,
                              "stage2_suite_driver: cannot write ~ts: ~p~n",
                              [Report, Reason])
            end;
        false ->
            io:format(standard_error,
                      "stage2_suite_driver: cannot encode the report~n", [])
    end.

%% Render a term compactly and clip it, so one huge assertion failure cannot
%% bury the rest of the report.
fmt(Term) ->
    try
        clip(lists:flatten(io_lib:format("~0p", [Term])), ?MAX_DETAIL)
    catch
        Class:Reason ->
            lists:flatten(io_lib:format("<unprintable ~s:~0p>", [Class, Reason]))
    end.

clip(Text, Max) when length(Text) =< Max ->
    Text;
clip(Text, Max) ->
    lists:sublist(Text, Max) ++ " ...[clipped]".

%%--------------------------------------------------------------------------
%% Scratch directories
%%--------------------------------------------------------------------------

%% A private, writable directory for the probes a suite compiles.
priv_dir(Suite) ->
    Base = case os:getenv("STAGE2_PRIV_BASE") of
               false -> tmp_base();
               "" -> tmp_base();
               Dir -> Dir
           end,
    Priv = filename:join(Base, atom_to_list(Suite) ++ "_priv"),
    ok = filelib:ensure_dir(filename:join(Priv, "x")),
    Priv.

tmp_base() ->
    case os:getenv("TMPDIR") of
        false -> "/tmp";
        "" -> "/tmp";
        Dir -> Dir
    end.
