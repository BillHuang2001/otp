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
%% beam_ssa_opt_diff_SUITE -- STAGE 1 of a randomized differential-equivalence
%% harness for the opt-in Stage-2 SSA compiler passes (`loop_opt', `fusion_opt',
%% `fbip_opt').
%%
%% THE IDEA
%% The opt-in passes are default-OFF (see `beam_ssa_opt:expand_opt/2'), so a
%% build with the flags is refutable against the build without them: compile the
%% SAME probe module twice - baseline `[binary,no_spawn_compiler_process]' and
%% all passes ON - call every exported function with the same argument battery
%% in both builds, and require the results to agree. Every call is executed in
%% a fresh process, so a probe's own mailbox or process dictionary cannot leak
%% between calls or between builds; a normal return is compared as `{ok,Value}'
%% and an exception as `{Class,Reason}' (the stacktrace is deliberately NOT part
%% of the comparison, mirroring `beam_ssa_opt_equiv_SUITE').
%%
%% STAGE 1 SCOPE
%% This is the minimal, hand-written first stage of the harness: two tiny probe
%% modules (every function ARITY-1, so one fixed battery drives all of them -
%% covering the loop strength reduction, a map extraction, a fresh record update
%% and two raisers) compiled under two option sets. It reports, per option set,
%% how many probe modules have a `Code' chunk differing from the baseline. A
%% later stage grows this into a random programme generator and many option
%% sets; the probe source, the battery and the option sets are the extension
%% points.
%%
%% The `Code' chunk is extracted by hand from the FOR1 container: a host
%% OTP-29 `beam_lib:chunks/2' cannot read it at all
%% (`{error,beam_lib,{unknown_chunk,...}}'). See `beam_chunks/1,2'.
%%
-module(beam_ssa_opt_diff_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([diff_equivalence/1]).

suite() ->
    [{ct_hooks,[ts_install_cth]},
     {timetrap,{minutes,10}}].

all() ->
    [{group,diff}].

groups() ->
    [{diff,[],[diff_equivalence]}].

init_per_suite(Config) ->
    Config.

end_per_suite(_Config) ->
    ok.

init_per_group(_GroupName, Config) ->
    Config.

end_per_group(_GroupName, Config) ->
    Config.

%% Compile every probe module under the baseline option set and under the
%% passes-ON option set, then compare every exported function on every battery
%% argument across the builds and report the counts.
diff_equivalence(Config) ->
    Sets = option_sets(),
    Dir = tmp_dir(Config),
    Files = [{ModBase, write_probe(Dir, ModBase, Source)}
             || {ModBase, Source} <- probes()],
    Builds = [{ModBase, Label, build(File, Opts)}
              || {ModBase, File} <- Files, {Label, Opts} <- Sets],
    Baseline = maps:from_list([{ModBase, Build}
                               || {ModBase, off, Build} <- Builds]),

    {Funs, Executions} = compare_builds(Sets, Builds, Baseline),
    report_digests(Sets, Builds),

    ct:pal("probe modules: ~w, exported functions: ~w, battery size: ~w, "
           "builds: ~w, total probe-function executions (compared calls): ~w "
           "(~w baseline/ON comparisons)",
           [length(Files), Funs, length(battery()), length(Builds),
            Executions, Executions div 2]),
    ok.

%% The two builds: the baseline (`off') and all opt-in Stage-2 passes ON.
%% `no_spawn_compiler_process' keeps the compilation in the test process.
option_sets() ->
    Base = [binary,no_spawn_compiler_process],
    [{off, Base},
     {on, Base ++ [loop_opt,fusion_opt,fbip_opt]}].

%% Call every exported arity-1 function of every probe module with every
%% battery argument, in every build, and assert that each build agrees exactly
%% with the baseline build. Returns `{ExportedFunctions, Executions}', where
%% `ExportedFunctions' counts the distinct exported functions of the probe
%% corpus and `Executions' counts every probe-function execution (baseline and
%% ON alike).
compare_builds(_Sets, Builds, Baseline) ->
    lists:foldl(
      fun({ModBase, Label, {Mod, Bin}}, {Funs0, Exec0}) ->
              {RefMod, RefBin} = maps:get(ModBase, Baseline),
              %% Load this build once so its export table is readable; `run/4'
              %% reloads before every call (the builds share the module name).
              load(Mod, Bin),
              Exports = exported_funs(Mod),
              Exec1 =
                  lists:foldl(
                    fun({Fun, _Arity}, ExecA) ->
                            lists:foldl(
                              fun(Arg, ExecB) ->
                                      Expected = run(RefMod, RefBin, Fun, [Arg]),
                                      Got = run(Mod, Bin, Fun, [Arg]),
                                      ?assertEqual(
                                         {ModBase, Fun, Arg, Expected},
                                         {ModBase, Fun, Arg, Got}),
                                      ExecB + 1
                              end, ExecA, battery())
                    end, Exec0, Exports),
              %% Count the probe corpus' exports once, from the baseline build.
              Funs1 = case Label of
                          off -> Funs0 + length(Exports);
                          _ -> Funs0
                      end,
              {Funs1, Exec1}
      end, {0, 0}, Builds).

%% The exported arity-1 functions of a probe module (`module_info/1' is an
%% arity-1 export too, so it is excluded by name).
exported_funs(Mod) ->
    [{F, A} || {F, A} <- Mod:module_info(exports), A =:= 1, F =/= module_info].

%% Report how many probe modules each option set compiles to a different `Code'
%% chunk than the baseline. Not asserted in Stage 1 - it shows that the option
%% set is not a no-op on this probe corpus.
report_digests(Sets, Builds) ->
    Baseline = maps:from_list([{ModBase, code_digest(Bin)}
                               || {ModBase, off, {_Mod, Bin}} <- Builds]),
    lists:foreach(
      fun({Label, Opts}) ->
              Changed =
                  [ModBase || {ModBase, L, {_Mod, Bin}} <- Builds,
                              L =:= Label,
                              code_digest(Bin) =/= maps:get(ModBase, Baseline)],
              ct:pal("option set ~p ~p: ~w of ~w probe module(s) have a Code "
                     "chunk differing from the baseline~s",
                     [Label, Opts, length(Changed), map_size(Baseline),
                      case Changed of
                          [] -> "";
                          _ -> ": " ++ lists:flatten(lists:join(", ", Changed))
                      end])
      end, Sets).

%% The argument battery. Deliberately mixes the shapes each probe's clauses
%% dispatch on: small and huge integers, a negative zero, the immediate float
%% and non-float atoms, proper and improper lists, maps and non-maps. Every
%% exported probe function is arity 1, so the same battery drives all of them.
battery() ->
    [0, 1, -1, 7, 2.5, -0.0, 1 bsl 200, -(1 bsl 200), not_a_number,
     [1, 2, 3], [1 | 2], #{a => 1}, #{}, [{a, 1}], not_a_map].

%% The probe corpus: two hand-written modules, every function arity 1.
probes() ->
    [{"beam_ssa_opt_diff_probe1", probe1()},
     {"beam_ssa_opt_diff_probe2", probe2()}].

%% A probe with one function per shape the opt-in passes look for: the loop
%% strength reduction (`X * 2' -> `X + X'), a `has_map_field' + extraction
%% (Fusion 5), a freshly allocated record updated immediately (FBIP) and two
%% raisers.
probe1() ->
    [%% A probe with one function per shape the opt-in passes rewrite.
     "-module(beam_ssa_opt_diff_probe1).\n",
     "-export([f1/1,f2/1,f3/1,f4/1,f5/1]).\n",
     "-record(r, {a = 0, b = 0}).\n",
     "\n",
     "%% loop strength reduction: X * 2 -> X + X\n",
     "f1(V) -> V * 2.\n",
     "\n",
     "%% a map-lookup guard plus an extraction of the same key (Fusion 5)\n",
     "f2(M) when is_map(M), is_map_key(a, M) ->\n",
     "    #{a := X} = M,\n",
     "    X;\n",
     "f2(_) -> no.\n",
     "\n",
     "%% a freshly allocated record, updated immediately (the FBIP shape)\n",
     "f3(V) ->\n",
     "    R0 = #r{a = V},\n",
     "    R1 = R0#r{b = 1},\n",
     "    {R1#r.a, R1#r.b}.\n",
     "\n",
     "%% a raiser: division by zero\n",
     "f4(V) when is_integer(V) -> V div 0;\n",
     "f4(_) -> not_int.\n",
     "\n",
     "%% another raiser: arithmetic on a non-number\n",
     "f5(V) -> V + 1.\n"].

%% A probe with the remaining shapes: a receive loop, a list comprehension and
%% a record-update loop. The loop bounds are capped so no battery argument can
%% spin for an unbounded time.
probe2() ->
    ["-module(beam_ssa_opt_diff_probe2).\n",
     "-export([g1/1,g2/1,g3/1]).\n",
     "-record(r, {a = 0, b = 0}).\n",
     "\n",
     "%% a receive loop: enqueue N messages and then drain them\n",
     "g1(N) when is_integer(N), N > 0, N < 20 ->\n",
     "    send(N),\n",
     "    drain(N, 0);\n",
     "g1(_) -> drained.\n",
     "\n",
     "send(0) -> ok;\n",
     "send(N) when is_integer(N), N > 0 -> self() ! {msg, N}, send(N - 1);\n",
     "send(_) -> ok.\n",
     "\n",
     "drain(0, Acc) -> Acc;\n",
     "drain(N, Acc) when is_integer(N), N > 0 ->\n",
     "    receive\n",
     "        {msg, X} -> drain(N - 1, Acc + X)\n",
     "    after 0 ->\n",
     "        Acc\n",
     "    end;\n",
     "drain(_, Acc) -> Acc.\n",
     "\n",
     "%% a list comprehension\n",
     "g2(L) -> [X * 2 || X <- L].\n",
     "\n",
     "%% a record-update loop\n",
     "g3(N) when is_integer(N), N >= 0, N < 20 -> g3_loop(N, #r{a = 0});\n",
     "g3(_) -> gone.\n",
     "\n",
     "g3_loop(0, R) -> {R#r.a, R#r.b};\n",
     "g3_loop(N, R) when is_integer(N), N > 0 -> g3_loop(N - 1, R#r{a = N});\n",
     "g3_loop(_, R) -> {R#r.a, R#r.b}.\n"].

%% Run `Mod:Fun(Args)' in the freshly loaded build. A normal return is
%% `{ok,Value}' and an exception `{Class,Reason}'; the stacktrace is not part
%% of the comparison. The call runs in a fresh process so that a probe's own
%% mailbox (the receive loop) cannot leak into the test process or between
%% builds.
run(Mod, Bin, Fun, Args) ->
    load(Mod, Bin),
    Ref = make_ref(),
    Parent = self(),
    _ = spawn(fun() ->
                      Result = try apply(Mod, Fun, Args) of
                                   Value -> {ok, Value}
                               catch
                                   Class:Reason -> {Class, Reason}
                               end,
                      Parent ! {Ref, Result}
              end),
    receive
        {Ref, Result} -> Result
    end.

%% Load `Bin' as `Mod', replacing any previous build of the same module name.
load(Mod, Bin) ->
    _ = code:purge(Mod),
    {module, Mod} = code:load_binary(Mod, atom_to_list(Mod) ++ ".erl", Bin).

build(File, Opts) ->
    case compile:file(File, [return_errors,return_warnings|Opts]) of
        {ok, Mod, Bin, []} ->
            {Mod, Bin};
        {ok, _Mod, _Bin, Warnings} ->
            ct:fail({compile_failed, File, [], Warnings});
        {error, Errors, Warnings} ->
            ct:fail({compile_failed, File, Errors, Warnings})
    end.

%% The MD5 of the `Code' chunk of a beam binary. Comparing digests (rather than
%% whole beams) is what makes "did this option set change the generated code of
%% this probe?" a meaningful question: the other chunks carry the compile
%% options themselves.
code_digest(Bin) ->
    case lists:keyfind(<<"Code">>, 1, beam_chunks(Bin)) of
        {<<"Code">>, Payload} -> erlang:md5(Payload);
        false -> {no_code_chunk, erlang:md5(Bin)}
    end.

%% Hand-extract a chunk from the FOR1 container: a host OTP-29
%% `beam_lib:chunks/2' cannot read the `Code' chunk at all
%% (`{error,beam_lib,{unknown_chunk,...}}'). EA IFF 85 layout:
%% "FOR1" | 32-bit big-endian total size | form type ("BEAM", *not* a sized
%% chunk) | chunks of 4-byte id | 32-bit big-endian size | payload | 0..3 pad.
beam_chunks(<<"FOR1", _Total:32, _Form:4/binary, Rest/binary>>) ->
    beam_chunks(Rest, []);
beam_chunks(Bin) ->
    ct:fail({not_a_beam_file, Bin}).

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

tmp_dir(Config) ->
    Base = case proplists:get_value(priv_dir, Config) of
               undefined ->
                   case os:getenv("TMPDIR") of
                       false -> "/tmp";
                       "" -> "/tmp";
                       EnvDir -> EnvDir
                   end;
               PrivDir ->
                   PrivDir
           end,
    Sub = "beam_ssa_opt_diff_" ++
        integer_to_list(erlang:unique_integer([positive])),
    Dir = filename:join(Base, Sub),
    ok = filelib:ensure_path(Dir),
    Dir.

write_probe(Dir, Mod, Source) ->
    File = filename:join(Dir, Mod ++ ".erl"),
    ok = file:write_file(File, Source),
    File.
