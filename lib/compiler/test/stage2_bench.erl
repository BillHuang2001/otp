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
%%
%% stage2_bench -- compile-throughput harness for the opt-in "Stage-2" passes.
%%
%% Drives *this* tree's compiler (built by lib/compiler/scripts/host_env.sh)
%% over a whole corpus in ONE Erlang session and measures the wall-clock cost of
%% compiling that corpus with each of the Stage-2 option sets:
%%
%%     off      (baseline)            loop_opt + loop_arith_licm
%%     fusion_opt                     fbip_opt                      all
%%
%% For every option set it reports min/median wall-clock over N repetitions,
%% the ratio against `off', and how many modules produced a different `Code'
%% chunk than the baseline -- i.e. how many modules the pass actually changed.
%%
%% Two corpora can be selected by the optional third positional argument (the
%% wrapper exposes it as CORPUS):
%%
%%     stdlib (default)  <root>/lib/stdlib/src/*.erl   96 modules
%%     all               <root>/lib/*/src/*.erl        ~772 modules
%%
%% The `all' corpus is the source of the full-lib/*/src reach numbers: it
%% compiles every library source file and adds every lib/*/include directory to
%% the include search path so that as many modules as possible build.  A handful
%% of files still fail for unrelated include/host reasons; they are recorded and
%% printed, and the measurement continues.
%%
%% Run it through test/measure_stage2_compile.sh; do not invoke it directly.
%% It needs (a) the host Erlang (an older release is fine) and (b) the rig
%% compiler as the FIRST `compile' on the code path: it hard-fails when
%% `code:which(compile)' is not the rig, because measuring the wrong compiler
%% silently produces meaningless pass counts.  The rig path is taken from the
%% RIG_EBIN environment variable exported by the wrapper script.
%%
%% Fairness notes:
%%   * All option sets compile the *same* file list, in the same order, in the
%%     same session, with only the Stage-2 flag atoms added/removed.  Every
%%     module in the corpus is compiled by the same compiler process, so module
%%     loading, code-server and GC costs are paid identically by all sets.
%%   * One full `off' pass is run first and discarded (warmup).  The measured
%%     repetitions then rotate the order in which the option sets are visited,
%%     so a monotone drift of the machine (turbo, other load) does not favour
%%     one set.  Reporting the *minimum* over repetitions is the standard way
%%     to suppress interference; the median is reported alongside it, and the
%%     raw per-repetition numbers are printed so the spread is visible.
%%   * The digest of the generated `Code' chunk is computed for every module in
%%     every measured pass, so the hashing cost is identical for all sets.
%%
%% Modules that fail to compile are counted and listed (per option set); they
%% are never silently dropped.  Harness errors (wrong compiler, empty corpus,
%% nothing compiled) exit non-zero; a pass that does not fire does NOT.
%%
-module(stage2_bench).

-export([main/0, main/1]).

%% Repetitions run when the wrapper does not say otherwise.
-define(DEFAULT_REPS, 5).

%% Chunk id of the generated code in a BEAM file.
-define(CODE_CHUNK, <<"Code">>).

-type digest() :: binary() | failed.
%% Which source corpus to measure.
-type corpus_kind() :: stdlib | all.
%% {wall-clock ms, per-module code digests, compile failures, changed count}
-type run() :: {non_neg_integer(),
                [{string(), digest()}],
                [{string(), term()}],
                non_neg_integer()}.

%% ===================================================================
%% Entry points
%% ===================================================================

-spec main() -> no_return().
main() ->
    main(init:get_plain_arguments()).

-spec main([string()]) -> no_return().
main(Args) ->
    {Reps, Root, Corpus} = parse_args(Args),
    ok = check_compiler(),
    Files = corpus(Corpus, Root),
    Base = base_opts(Root, Corpus),
    print_header(Root, Reps, Corpus, Files),
    Ref = warmup(Files, Base),
    Results = measure(Reps, Files, Base, Ref),
    report(Results),
    print_raw(Results),
    %% halt/1 does not flush the io queue: without {flush,true} the table can be
    %% lost when stdout is a file or a pipe.
    erlang:halt(0, [{flush, true}]).

%% ===================================================================
%% Argument / environment handling
%% ===================================================================

-spec parse_args([string()]) -> {pos_integer(), string(), corpus_kind()}.
parse_args([]) ->
    {?DEFAULT_REPS, ".", stdlib};
parse_args([Reps]) ->
    {to_reps(Reps), ".", stdlib};
parse_args([Reps, Root | Rest]) ->
    {to_reps(Reps), Root, to_corpus(Rest)}.

%% The third positional argument names the corpus; an omitted argument selects
%% the default `stdlib' corpus, so `-extra REPS ROOT' stays valid.
-spec to_corpus([string()]) -> corpus_kind().
to_corpus([]) -> stdlib;
to_corpus(["stdlib" | _]) -> stdlib;
to_corpus(["all" | _]) -> all;
to_corpus([Other | _]) ->
    die("unknown corpus " ++ Other ++ "; expected `stdlib' or `all'").

-spec to_reps(string()) -> pos_integer().
to_reps(Str) ->
    try list_to_integer(Str) of
        N when N >= 1 -> N;
        N -> die("repetitions must be >= 1, got " ++ integer_to_list(N))
    catch
        error:badarg -> die("repetitions must be an integer, got " ++ Str)
    end.

%% The wrapper exports RIG_EBIN. Refuse to run against any other compiler: an
%% escript or a missing -pa would silently test the *host* OTP compiler, whose
%% behaviour says nothing about this tree's opt-in passes.
-spec check_compiler() -> ok.
check_compiler() ->
    Rig = os:getenv("RIG_EBIN"),
    Which = code:which(compile),
    case Rig of
        false ->
            die("RIG_EBIN is not set; run test/measure_stage2_compile.sh "
                "instead of stage2_bench directly (compile module is "
                ++ Which ++ ")");
        _ ->
            case lists:prefix(Rig, Which) of
                true ->
                    ok;
                false ->
                    die("wrong compiler: code:which(compile) = " ++ Which
                        ++ " is not under RIG_EBIN=" ++ Rig)
            end
    end.

-spec base_opts(string(), corpus_kind()) -> [compile:option()].
base_opts(Root, Kind) ->
    [binary, return | [{i, Dir} || Dir <- include_dirs(Root, Kind)]].

%% The three includes every corpus needs, plus (for `all') every lib/*/include
%% directory, so that the library modules find their own application headers.
-spec include_dirs(string(), corpus_kind()) -> [string()].
include_dirs(Root, Kind) ->
    Base = [filename:join([Root, "lib", "stdlib", "include"]),
            filename:join([Root, "lib", "kernel", "include"]),
            filename:join([Root, "lib", "compiler", "src"])],
    case Kind of
        stdlib ->
            Base;
        all ->
            Extra = lists:sort(
                      filelib:wildcard(
                        filename:join([Root, "lib", "*", "include"]))),
            Base ++ [Dir || Dir <- Extra, not lists:member(Dir, Base)]
    end.

-spec corpus(corpus_kind(), string()) -> [string()].
corpus(Kind, Root) ->
    Pattern = corpus_pattern(Kind, Root),
    case lists:sort(filelib:wildcard(Pattern)) of
        [] -> die("empty corpus: no files matching " ++ Pattern);
        Files -> Files
    end.

-spec corpus_pattern(corpus_kind(), string()) -> string().
corpus_pattern(stdlib, Root) ->
    filename:join([Root, "lib", "stdlib", "src", "*.erl"]);
corpus_pattern(all, Root) ->
    filename:join([Root, "lib", "*", "src", "*.erl"]).

%% The measured option sets, in their canonical order.
-spec sets() -> [{string(), [compile:option()]}].
sets() ->
    LICM = [loop_opt, loop_arith_licm],
    [{"off", []},
     {"loop_opt+loop_arith_licm", LICM},
     {"fusion_opt", [fusion_opt]},
     {"fbip_opt", [fbip_opt]},
     {"all", [loop_opt, loop_arith_licm, fusion_opt, fbip_opt]}].

%% ===================================================================
%% Measurement
%% ===================================================================

%% One discarded `off' pass: loads and JITs the compiler, warms the code
%% server, and provides the reference `Code' digests for the change counts.
-spec warmup([string()], [compile:option()]) -> [{string(), digest()}].
warmup(Files, Base) ->
    io:format("warmup: compiling ~w modules with OFF (discarded) ...~n",
              [length(Files)]),
    {Ms, Digests, Fails} = pass(Files, Base, []),
    print_failures("warmup(off)", Fails),
    case [File || {File, D} <- Digests, D =/= failed] of
        [] -> die("nothing compiled: all " ++ integer_to_list(length(Files))
                  ++ " module(s) failed");
        _ -> ok
    end,
    io:format("warmup: ~w ms (~.1f ms/module)~n~n",
              [Ms, Ms / length(Files)]),
    Digests.

-spec measure(pos_integer(), [string()], [compile:option()],
              [{string(), digest()}]) -> [{string(), [run()]}].
measure(Reps, Files, Base, Ref) ->
    All = sets(),
    lists:foldl(
      fun(Rep, Acc) ->
              Order = rotate(All, Rep - 1),
              io:format("repetition ~w/~w, option order: ~s~n",
                        [Rep, Reps, order_str(Order)]),
              lists:foldl(
                fun({Name, Extra}, Acc1) ->
                        {Ms, Digests, Fails} = pass(Files, Base, Extra),
                        Changed = changed(Ref, Digests),
                        io:format("  ~-26s ~8w ms  (~w changed, ~w failed)~n",
                                  [Name, Ms, Changed, length(Fails)]),
                        io:format("  ~-26s   failed: ~s~n",
                                  ["", failure_files(Fails)]),
                        add_run(Name, {Ms, Digests, Fails, Changed}, Acc1)
                end, Acc, Order)
      end, [], lists:seq(1, Reps)).

-spec add_run(string(), run(), [{string(), [run()]}]) -> [{string(), [run()]}].
add_run(Name, Run, Acc) ->
    case lists:keyfind(Name, 1, Acc) of
        false -> Acc ++ [{Name, [Run]}];
        {Name, Runs} ->
            lists:keyreplace(Name, 1, Acc, {Name, Runs ++ [Run]})
    end.

%% Timed compilation of the whole corpus with `Base ++ Extra'.
-spec pass([string()], [compile:option()], [compile:option()]) ->
          {non_neg_integer(), [{string(), digest()}], [{string(), term()}]}.
pass(Files, Base, Extra) ->
    Opts = Base ++ Extra,
    T0 = erlang:monotonic_time(millisecond),
    {Digests, Fails} = compile_files(Files, Opts),
    T1 = erlang:monotonic_time(millisecond),
    {T1 - T0, Digests, Fails}.

-spec compile_files([string()], [compile:option()]) ->
          {[{string(), digest()}], [{string(), term()}]}.
compile_files(Files, Opts) ->
    {RevDigests, RevFails} =
        lists:foldl(
          fun(File, {Ds, Fs}) ->
                  case compile_one(File, Opts) of
                      {ok, Digest} -> {[{File, Digest} | Ds], Fs};
                      {failed, Why} -> {[{File, failed} | Ds], [{File, Why} | Fs]}
                  end
          end, {[], []}, Files),
    {lists:reverse(RevDigests), lists:reverse(RevFails)}.

-spec compile_one(string(), [compile:option()]) -> {ok, digest()} | {failed, term()}.
compile_one(File, Opts) ->
    try compile:file(File, Opts) of
        {ok, _Mod, Bin} -> {ok, code_digest(Bin)};
        {ok, _Mod, Bin, _Warnings} -> {ok, code_digest(Bin)};
        {error, Errors, _Warnings} -> {failed, first_error(Errors)};
        Other -> {failed, {unexpected_result, Other}}
    catch
        Class:Reason -> {failed, {Class, Reason}}
    end.

%% Hand-extract a chunk from the FOR1 container: an OTP-29 host's
%% beam_lib:chunks/2 cannot read the `code' chunk at all
%% ({error,beam_lib,{unknown_chunk, ...}}).  EA IFF 85 layout:
%% "FOR1" | 32-bit big-endian total size | form type ("BEAM", *not* a sized
%% chunk) | chunks of 4-byte id | 32-bit big-endian size | payload | 0..3 pad.
-spec code_digest(binary()) -> digest().
code_digest(Bin) ->
    case lists:keyfind(?CODE_CHUNK, 1, beam_chunks(Bin)) of
        {?CODE_CHUNK, Payload} -> erlang:md5(Payload);
        false -> {no_code_chunk, erlang:md5(Bin)}
    end.

-spec beam_chunks(binary()) -> [{binary(), binary()}].
beam_chunks(<<"FOR1", _Total:32, _Form:4/binary, Rest/binary>>) ->
    beam_chunks(Rest, []);
beam_chunks(Bin) ->
    die("not a BEAM file: " ++ io_lib:format("~P", [Bin, 6])).

beam_chunks(<<>>, Acc) ->
    lists:reverse(Acc);
beam_chunks(<<Id:4/binary, Size:32, Rest0/binary>>, Acc) ->
    case Rest0 of
        <<Payload:Size/binary, Rest1/binary>> ->
            Pad = (4 - (Size rem 4)) rem 4,
            case Rest1 of
                <<_:Pad/binary, Rest2/binary>> ->
                    beam_chunks(Rest2, [{Id, Payload} | Acc]);
                _Tail ->
                    %% last chunk written without its alignment padding
                    lists:reverse([{Id, Payload} | Acc])
            end;
        _Short ->
            lists:reverse(Acc)
    end;
beam_chunks(_Trailing, Acc) ->
    %% fewer than 8 bytes left: end-of-file alignment padding
    lists:reverse(Acc).

%% Number of modules whose `Code' digest differs from the OFF reference.
-spec changed([{string(), digest()}], [{string(), digest()}]) -> non_neg_integer().
changed(Ref, Digests) ->
    length([File || {File, D} <- Digests,
                    D =/= failed,
                    not same_digest(D, File, Ref)]).

same_digest(D, File, Ref) ->
    case lists:keyfind(File, 1, Ref) of
        {_, D} -> true;
        _ -> false
    end.

-spec first_error(term()) -> term().
first_error([First | _]) -> First;
first_error(Other) -> Other.

-spec failure_files([{string(), term()}]) -> iolist().
failure_files([]) ->
    "none";
failure_files(Fails) ->
    lists:join(", ", [filename:basename(F) || {F, _} <- Fails]).

%% ===================================================================
%% Reporting
%% ===================================================================

-spec print_header(string(), pos_integer(), corpus_kind(), [string()]) -> ok.
print_header(Root, Reps, Kind, Files) ->
    io:format(
      "~n======================================================================~n"
      " Stage-2 compile-throughput harness (ratios are the only signal; this~n"
      " host is shared and loaded, so absolute times vary between runs)~n"
      "======================================================================~n"
      " host OTP        : ~s (erts ~s)~n"
      " compiler module : ~s~n"
      " corpus          : ~w modules: ~s~n"
      " repetitions     : ~w measured + 1 discarded warmup, rotated order~n"
      " option sets     : ~s~n"
      "======================================================================~n",
      [erlang:system_info(otp_release), erlang:system_info(version),
       code:which(compile), length(Files), corpus_pattern(Kind, Root), Reps,
       order_str(sets())]),
    ok.

-spec report([{string(), [run()]}]) -> ok.
report(Results) ->
    io:format("~n"
              " set                          min_ms   med_ms   max_ms  "
              "ratio_min  ratio_med  changed  failed~n"
              "--------------------------------------------------------------------"
              "--------------------~n", []),
    OffRuns = runs("off", Results),
    MinOff = lists:min([Ms || {Ms, _, _, _} <- OffRuns]),
    MedOff = median([Ms || {Ms, _, _, _} <- OffRuns]),
    lists:foreach(
      fun({Name, Runs}) ->
              Times = [Ms || {Ms, _, _, _} <- Runs],
              Min = lists:min(Times),
              Med = median(Times),
              Max = lists:max(Times),
              Changed = fmt_counts([C || {_, _, _, C} <- Runs]),
              Failed = fmt_counts([length(Fs) || {_, _, Fs, _} <- Runs]),
              io:format(" ~-26s ~8w ~8w ~8w  ~9.3f  ~9.3f  ~7s  ~6s~n",
                        [Name, Min, Med, Max, Min / MinOff, Med / MedOff,
                         Changed, Failed])
      end, Results),
    io:format("--------------------------------------------------------------------"
              "--------------------~n"
              " ratios are against the `off' baseline (min over mins, median over "
              "medians)~n"
              " changed = modules whose generated `Code' chunk differs from the "
              "OFF reference build~n"
              " counts shown as `a/b/c' mean the value was not stable across "
              "repetitions~n", []).

%% Per-repetition numbers, in measurement order: this is the observed spread.
-spec print_raw([{string(), [run()]}]) -> ok.
print_raw(Results) ->
    io:format("~n raw wall-clock per measured repetition (ms), in order:~n", []),
    lists:foreach(
      fun({Name, Runs}) ->
              io:format("   ~-26s ~p~n", [Name, [Ms || {Ms, _, _, _} <- Runs]])
      end, Results),
    io:format("~n").

%% A single stable number, or "a/b/.." when the reps disagree.
-spec fmt_counts([non_neg_integer()]) -> string().
fmt_counts(Counts) ->
    case lists:usort(Counts) of
        [N] -> integer_to_list(N);
        Ns -> lists:flatten(lists:join("/", [integer_to_list(N) || N <- Ns]))
    end.

-spec runs(string(), [{string(), [run()]}]) -> [run()].
runs(Name, Results) ->
    case lists:keyfind(Name, 1, Results) of
        {_, Runs} -> Runs;
        false -> die("no runs for option set " ++ Name)
    end.

-spec median([non_neg_integer()]) -> non_neg_integer().
median(Times) ->
    Sorted = lists:sort(Times),
    %% lower median; for an odd count this is the exact middle
    lists:nth((length(Sorted) + 1) div 2, Sorted).

-spec print_failures(string(), [{string(), term()}]) -> ok.
print_failures(_What, []) ->
    ok;
print_failures(What, Fails) ->
    io:format("!!! ~s: ~w module(s) failed to compile:~n", [What, length(Fails)]),
    lists:foreach(fun({F, Why}) ->
                          io:format("!!!   ~s: ~P~n", [F, Why, 8])
                  end, Fails),
    ok.

-spec order_str([{string(), [compile:option()]}]) -> iolist().
order_str(Sets) ->
    lists:join(" -> ", [Name || {Name, _} <- Sets]).

-spec rotate([T], non_neg_integer()) -> [T].
rotate(List, N) ->
    Len = length(List),
    {A, B} = lists:split(N rem Len, List),
    B ++ A.

-spec die(string()) -> no_return().
die(Msg) ->
    io:format(standard_error, "stage2_bench: harness error: ~s~n", [Msg]),
    erlang:halt(1, [{flush, true}]).
