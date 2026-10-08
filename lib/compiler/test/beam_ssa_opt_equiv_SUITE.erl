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
%% A reusable behavioural-equivalence correctness gate for the opt-in SSA
%% passes (beam_ssa_loop_opt / beam_ssa_fus / beam_ssa_fbip), plus a small
%% self-contained benchmark of the same in-tree compiler.
%%
%% The gate compiles the SAME probe module twice - once with the pass(es)
%% OFF (the base option list) and once with them ON - and then calls every
%% probe function with a battery of arguments in both builds. Every build
%% must return exactly the same `{ok,Value}' / `{Class,Reason}' results as
%% the OFF build, which is the semantics reference (the stacktrace is
%% deliberately NOT part of the comparison). This is the safety net the
%% rewrite plan mandates: an opt-in pass may not change observable
%% behaviour, only the generated code.
%%
%% The passes are DEFAULT-OFF (see beam_ssa_opt:expand_opt/2), so for each
%% case the "OFF" build is `[binary,no_spawn_compiler_process]' and the "ON"
%% build adds the pass flag(s). `no_spawn_compiler_process' keeps the
%% compilation in the test process.
%%
%% The benchmark compiles a small hand-written kernel probe with all three
%% passes OFF vs ON, checks that both builds behave identically (reusing the
%% equivalence machinery), and measures compilation time plus the execution
%% time of each kernel (several batches, minimum taken to reduce noise). It
%% hard-asserts only that the ON build is not catastrophically slower
%% (`ON =< OFF * 2'); whether the passes actually make a kernel faster is
%% reported, not asserted, because the opt-in passes may legitimately not
%% change a given kernel at all.
%%
-module(beam_ssa_opt_equiv_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([loop_opt_equivalence/1,
         fusion_opt_equivalence/1,
         fbip_opt_equivalence/1,
         all_passes_equivalence/1,
         benchmark/1]).

suite() ->
    [{ct_hooks,[ts_install_cth]},
     {timetrap,{minutes,5}}].

all() ->
    [{group,p}].

groups() ->
    [{p,[],
      [loop_opt_equivalence,
       fusion_opt_equivalence,
       fbip_opt_equivalence,
       all_passes_equivalence,
       benchmark]}].

init_per_suite(Config) ->
    Config.

end_per_suite(_Config) ->
    ok.

init_per_group(_GroupName, Config) ->
    Config.

end_per_group(_GroupName, Config) ->
    Config.

%%============================================================================
%% Test cases
%%============================================================================

%% The loop pass (beam_ssa_loop_opt) individually, ON vs OFF.
loop_opt_equivalence(Config) ->
    equivalence_case(Config, "beam_ssa_opt_equiv_loop", [loop_opt]).

%% The map-lookup fusion pass (beam_ssa_fus) individually, ON vs OFF.
fusion_opt_equivalence(Config) ->
    equivalence_case(Config, "beam_ssa_opt_equiv_fusion", [fusion_opt]).

%% The FBIP pass (beam_ssa_fbip) individually, ON vs OFF.
fbip_opt_equivalence(Config) ->
    equivalence_case(Config, "beam_ssa_opt_equiv_fbip", [fbip_opt]).

%% All three opt-in passes together, ON vs OFF.
all_passes_equivalence(Config) ->
    equivalence_case(Config, "beam_ssa_opt_equiv_all", passes()).

%%============================================================================
%% (b) Benchmark
%%============================================================================

%% Compile the kernel probe with all three passes OFF and ON, check that both
%% builds behave identically, and measure compilation time as well as the
%% execution time of every kernel. Reports the per-kernel numbers and a final
%% "ON faster: yes/no" verdict; hard-asserts only that the ON build is not
%% catastrophically slower than the OFF build.
benchmark(Config) ->
    ModBase = "beam_ssa_opt_bench",
    Dir = tmp_dir(Config),
    File = write_probe(Dir, ModBase, bench_probe(ModBase)),

    {OffMs, {OffMod, OffBin}} = compile_timed(File, base_opts()),
    {OnMs, {OnMod, OnBin}} = compile_timed(File, base_opts() ++ passes()),

    %% (i) The two builds must return exactly the same results/exceptions.
    check_battery([{off, OffMod, OffBin}, {on, OnMod, OnBin}],
                  bench_cases()),

    %% (ii) Measure the kernels and guard each one against a pathological
    %% regression (a loose `ON =< OFF * 2' bound).
    Rows = [measure_kernel(OffMod, OffBin, OnMod, OnBin, K)
            || K <- bench_kernels()],

    Changed = disasm(OffBin) =/= disasm(OnBin),
    TotalOffUs = lists:sum([OffUs || {_, _, OffUs, _} <- Rows]),
    TotalOnUs = lists:sum([OnUs || {_, _, _, OnUs} <- Rows]),
    AnyFaster = lists:any(fun({_, _, OffUs, OnUs}) -> OnUs < OffUs end, Rows),
    Verdict = case AnyFaster of true -> "yes"; false -> "no" end,

    ct:pal("~n=== beam_ssa_opt benchmark (~s) ===", [ModBase]),
    ct:pal("passes changed generated code: ~p", [Changed]),
    ct:pal("compilation time  OFF: ~.2f ms   ON: ~.2f ms", [OffMs, OnMs]),
    ct:pal("~-12s ~11s ~11s ~10s",
           ["kernel", "OFF ms", "ON ms", "speedup"]),
    lists:foreach(fun({Name, Arity, OffUs, OnUs}) ->
                          ct:pal("~-12s ~11.3f ~11.3f ~10s",
                                 [name_text(Name, Arity),
                                  OffUs / 1000, OnUs / 1000,
                                  speedup_text(OffUs, OnUs)])
                  end, Rows),
    ct:pal("total         ~11.3f ~11.3f ~10s",
           [TotalOffUs / 1000, TotalOnUs / 1000,
            speedup_text(TotalOffUs, TotalOnUs)]),
    ct:pal("VERDICT: ON faster: ~s", [Verdict]),
    ok.

%% Compile the equivalence probe with the pass options `PassOpts' OFF and ON
%% and check that both builds behave identically. Also reports (but does not
%% assert) whether the option actually changed the generated code.
equivalence_case(Config, ModBase, PassOpts) ->
    Source = equiv_probe(ModBase),
    Sets = [{off, base_opts()},
            {on, base_opts() ++ PassOpts}],
    Builds = check_equivalence(Config, ModBase, Source, Sets),
    _ = report_change(ModBase, PassOpts, Builds),
    ok.

%%============================================================================
%% The equivalence gate
%%============================================================================

%% The base option list shared by every build: compile to memory and do the
%% compilation in the calling (test) process.
base_opts() ->
    [binary,no_spawn_compiler_process].

%% The opt-in passes exercised here.
passes() ->
    [loop_opt,fusion_opt,fbip_opt].

%% Compile `ProbeSource' (whose module basename is `ProbeSourceBaseName')
%% once per `{Label,CompileOpts}' in `OptionSets', then check that every build
%% returns exactly the same results and exceptions as the first build for the
%% whole battery (see `probe_cases/1').
%%
%% Returns the builds as `{Label,Mod,Bin,Code}' (Code being the beam_disasm
%% function list), in the same order as `OptionSets'.
check_equivalence(Config, ProbeSourceBaseName, ProbeSource, OptionSets) ->
    Dir = tmp_dir(Config),
    File = write_probe(Dir, ProbeSourceBaseName, ProbeSource),
    Builds = [{Label, build(File, Opts)} || {Label,Opts} <- OptionSets],
    check_battery([{Label, Mod, Bin} || {Label,{Mod,Bin}} <- Builds],
                  probe_cases(ProbeSourceBaseName)),
    [{Label, Mod, Bin, disasm(Bin)} || {Label,{Mod,Bin}} <- Builds].

%% The functions and argument sets the two probes are called with.
probe_cases("beam_ssa_opt_bench" ++ _) -> bench_cases();
probe_cases(_) -> equiv_cases().

%% Every probe function is called with every argument set in both builds. The
%% result of the first (reference) build is the expected one; every other
%% build must agree exactly. `run/4' reports a normal return as `{ok,Value}'
%% and an exception as `{Class,Reason}'; the stacktrace is not compared.
check_battery([{_RefLabel, RefMod, RefBin} | _] = Builds, Cases) ->
    lists:foreach(
      fun({Fun, ArgSets}) ->
              Expected = [{Args, run(RefMod, RefBin, Fun, Args)}
                          || Args <- ArgSets],
              lists:foreach(
                fun({Label, Mod, Bin}) ->
                        Got = [{Args, run(Mod, Bin, Fun, Args)}
                               || Args <- ArgSets],
                        ?assertEqual({Label, Fun, Expected}, {Label, Fun, Got})
                end, Builds)
      end, Cases),
    ok.

%%============================================================================
%% Probes: behavioural equivalence
%%============================================================================

%% A module exercising every construct the opt-in passes look for: a receive
%% loop, a map-lookup guard with extraction, record construction/update, a
%% local freshly-allocating function whose result is updated (the FBIP
%% shape), a list comprehension, integer arithmetic loops with `* 2', and two
%% functions that raise (a `badarith' and a `function_clause').
equiv_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([recv/1,mp/1,rec/1,rec_fresh/1,rec_loop/1,mk_fresh/1,\n",
     "         lc/1,arith/1,sum/1,ex/1,fc/1]).\n",
     "-record(r, {a = 0, b = 0}).\n",
     "\n",
     "%% A receive loop: enqueue N messages and then drain them.\n",
     "recv(N) ->\n",
     "    send(N),\n",
     "    recv_loop(N, 0).\n",
     "\n",
     "send(0) -> ok;\n",
     "send(N) when is_integer(N), N > 0 -> self() ! {msg, N}, send(N - 1);\n",
     "send(_) -> ok.\n",
     "\n",
     "recv_loop(0, Acc) -> Acc;\n",
     "recv_loop(N, Acc) when is_integer(N), N > 0 ->\n",
     "    receive\n",
     "        {msg, X} -> recv_loop(N - 1, Acc + X)\n",
     "    after 0 ->\n",
     "        Acc\n",
     "    end;\n",
     "recv_loop(_, Acc) -> Acc.\n",
     "\n",
     "%% A map-lookup guard plus an extraction of the same key.\n",
     "mp(M) when is_map(M), is_map_key(a, M) ->\n",
     "    #{a := V} = M,\n",
     "    V;\n",
     "mp(_) -> no.\n",
     "\n",
     "%% Record construction and update.\n",
     "rec(X) ->\n",
     "    R0 = #r{a = X, b = X + 1},\n",
     "    R1 = R0#r{b = X + 2},\n",
     "    {R1#r.a, R1#r.b}.\n",
     "\n",
     "%% A local function that always allocates freshly: the FBIP shape.\n",
     "mk_fresh(X) -> #r{a = X}.\n",
     "\n",
     "rec_fresh(X) ->\n",
     "    Y = X + 2,\n",
     "    R0 = mk_fresh(X),\n",
     "    R1 = R0#r{b = Y},\n",
     "    {R1#r.a, R1#r.b}.\n",
     "\n",
     "%% A record-update loop.\n",
     "rec_loop(N) -> rec_loop(N, #r{a = 0}).\n",
     "rec_loop(0, R) -> {R#r.a, R#r.b};\n",
     "rec_loop(N, R) when is_integer(N), N > 0 -> rec_loop(N - 1, R#r{a = N});\n",
     "rec_loop(_, R) -> {R#r.a, R#r.b}.\n",
     "\n",
     "%% A list comprehension.\n",
     "lc(L) -> [X * 2 || X <- L].\n",
     "\n",
     "%% An integer arithmetic loop (the `* 2' is what strength reduction\n",
     "%% looks for).\n",
     "arith(N) when is_integer(N), N >= 0 -> arith_loop(N, 0);\n",
     "arith(N) -> arith_loop(0, N).\n",
     "\n",
     "arith_loop(0, Acc) -> Acc;\n",
     "arith_loop(N, Acc) when is_integer(N), N > 0 -> arith_loop(N - 1, Acc + N * 2);\n",
     "arith_loop(_, Acc) -> Acc.\n",
     "\n",
     "%% A hand-written list sum.\n",
     "sum(L) -> sum(L, 0).\n",
     "sum([H | T], Acc) -> sum(T, Acc + H);\n",
     "sum([], Acc) -> Acc.\n",
     "\n",
     "%% Raisers: a badarith and a function_clause.\n",
     "ex(X) -> 1 / X.\n",
     "\n",
     "fc(X) -> fc_clause(X).\n",
     "fc_clause(a) -> ok.\n"].

%% Argument sets for the equivalence probe. The inputs deliberately include
%% both the success and the error/exception paths (a non-map, a non-integer,
%% a division by zero, a function_clause).
equiv_cases() ->
    [{recv, [[0], [3], [-1]]},
     {mp, [[#{a => 1}], [#{}], [[{a, 1}]], [not_a_map]]},
     {rec, [[1], [42], [x]]},
     {rec_fresh, [[1], [42], [-7], [x]]},
     {rec_loop, [[0], [1], [5], [-2]]},
     {lc, [[[1, 2, 3]], [[]], ["abc"], [[a, b]]]},
     {arith, [[0], [5], [-3], [x]]},
     {sum, [[[1, 2, 3]], [[]], [[10, -3, 2]]]},
     {ex, [[2], [0], [x]]},
     {fc, [[a], [b]]}].

%%============================================================================
%% Probes: benchmark kernels
%%============================================================================

%% A handful of hand-written compute kernels: a list sum, a tail-recursive
%% count, tak/3, naive fib/1, a map-lookup kernel, a record-update loop and
%% an arithmetic loop with `X * 2'.
bench_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([sum/1,count/1,tak/3,fib/1,maplookup/1,rec_upd/1,arith/1]).\n",
     "-record(r, {a = 0, b = 0}).\n",
     "\n",
     "%% Sum of a list.\n",
     "sum(L) -> sum(L, 0).\n",
     "sum([H | T], Acc) -> sum(T, Acc + H);\n",
     "sum([], Acc) -> Acc.\n",
     "\n",
     "%% Tail-recursive counting.\n",
     "count(N) -> count(N, 0).\n",
     "count(0, Acc) -> Acc;\n",
     "count(N, Acc) when is_integer(N), N > 0 -> count(N - 1, Acc + 1);\n",
     "count(_, Acc) -> Acc.\n",
     "\n",
     "%% The classic Takeuchi function.\n",
     "tak(X, Y, Z) ->\n",
     "    if\n",
     "        X =< Y -> Z;\n",
     "        true -> tak(tak(X - 1, Y, Z), tak(Y - 1, Z, X), tak(Z - 1, X, Y))\n",
     "    end.\n",
     "\n",
     "%% Naive Fibonacci.\n",
     "fib(0) -> 0;\n",
     "fib(1) -> 1;\n",
     "fib(N) when is_integer(N), N > 1 -> fib(N - 1) + fib(N - 2);\n",
     "fib(N) -> N.\n",
     "\n",
     "%% A map-lookup kernel: an is_map_key/2 guard plus an extraction per\n",
     "%% step.\n",
     "maplookup(N) -> maplookup(N, #{a => 1, b => 2, c => 3}).\n",
     "maplookup(0, M) -> M;\n",
     "maplookup(N, M) when is_integer(N), N > 0 ->\n",
     "    case is_map_key(a, M) of\n",
     "        true ->\n",
     "            #{a := V} = M,\n",
     "            maplookup(N - 1, M#{a := V + 1});\n",
     "        false ->\n",
     "            M\n",
     "    end;\n",
     "maplookup(_, M) -> M.\n",
     "\n",
     "%% A record-update loop.\n",
     "rec_upd(N) -> rec_upd(N, #r{a = 0, b = 0}).\n",
     "rec_upd(0, R) -> R#r.a + R#r.b;\n",
     "rec_upd(N, R) when is_integer(N), N > 0 -> rec_upd(N - 1, R#r{a = N});\n",
     "rec_upd(_, R) -> R#r.a + R#r.b.\n",
     "\n",
     "%% An arithmetic loop with X * 2.\n",
     "arith(N) -> arith(N, 0).\n",
     "arith(0, Acc) -> Acc;\n",
     "arith(N, Acc) when is_integer(N), N > 0 -> arith(N - 1, Acc + N * 2);\n",
     "arith(_, Acc) -> Acc.\n"].

%% {Fun, Args, Repeats}: each kernel is run `Repeats' times per build and the
%% minimum is reported, which removes most scheduler noise.
bench_kernels() ->
    [{sum, [lists:seq(1, 300000)], 5},
     {count, [1500000], 5},
     {tak, [23, 12, 6], 5},
     {fib, [28], 5},
     {maplookup, [80000], 5},
     {rec_upd, [1500000], 5},
     {arith, [800000], 5}].

%% The small argument sets used for the behavioural check of the kernel probe.
bench_cases() ->
    [{sum, [[1, 2, 3, 4, 5]]},
     {count, [[100]]},
     {tak, [[10, 5, 2]]},
     {fib, [[10]]},
     {maplookup, [[100]]},
     {rec_upd, [[100]]},
     {arith, [[100]]}].

%%============================================================================
%% Running, measuring, compiling
%%============================================================================

%% Run `Mod:Fun(Args)' in the code that was just loaded. The result is
%% `{ok,Value}' for a normal return and `{Class,Reason}' for an exception;
%% the stacktrace is deliberately not part of the comparison. The call runs
%% in a fresh process so that a probe's own mailbox (the receive loop) cannot
%% interfere with the test process or leak between builds.
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

%% Run one kernel `Reps' times and return the minimum wall-clock time in
%% microseconds, so a single scheduling hiccup does not skew the result.
time_kernel(Mod, Bin, Fun, Args, Reps) ->
    load(Mod, Bin),
    Times = [begin
                 {Us, _Result} = timer:tc(fun() -> apply(Mod, Fun, Args) end),
                 Us
             end || _ <- lists:seq(1, Reps)],
    lists:min(Times).

%% Measure a kernel in both builds and guard against a catastrophic
%% regression: the ON build must not take more than twice the OFF time.
measure_kernel(OffMod, OffBin, OnMod, OnBin, {Fun, Args, Reps}) ->
    Arity = length(Args),
    OffUs = time_kernel(OffMod, OffBin, Fun, Args, Reps),
    OnUs = time_kernel(OnMod, OnBin, Fun, Args, Reps),
    case OnUs =< OffUs * 2 of
        true ->
            ok;
        false ->
            ct:fail({kernel_slower_with_passes, {Fun, Arity},
                     {off_us, OffUs}, {on_us, OnUs}})
    end,
    {Fun, Arity, OffUs, OnUs}.

%% Compile `File' with `Opts' `reps' times and return the minimum compile
%% time in milliseconds together with one build. Repeating removes the
%% first-compile warm-up from the reported number.
compile_timed(File, Opts) ->
    Timed = [begin {Us, B} = timer:tc(fun() -> build(File, Opts) end),
                  {Us, B}
             end || _ <- lists:seq(1, 3)],
    MinUs = lists:min([Us || {Us, _} <- Timed]),
    {_LastUs, Build} = lists:last(Timed),
    {MinUs / 1000, Build}.

build(File, Opts) ->
    case compile:file(File, [return_errors,return_warnings|Opts]) of
        {ok, Mod, Bin, []} ->
            {Mod, Bin};
        {ok, _Mod, _Bin, Warnings} ->
            ct:fail({compile_failed, File, [], Warnings});
        {error, Errors, Warnings} ->
            ct:fail({compile_failed, File, Errors, Warnings})
    end.

%% The code of one build, i.e. the list of `{function,Name,Arity,Label,Is}'
%% entries. The compile_info chunk is not part of it, so builds with
%% different options can be compared directly.
disasm(Bin) ->
    case beam_disasm:file(Bin) of
        {beam_file, _Mod, _Exports, _Attrs, _CompileInfo, Code} ->
            Code;
        {error, Mod, Reason} ->
            ct:fail({beam_disasm_failed, Mod, Reason})
    end.

%% Report (and return) whether the option actually changed the generated
%% code, which is what shows that the pass fired.
report_change(ModBase, PassOpts, Builds) ->
    [{off, _, _, OffCode}, {on, _, _, OnCode}] = Builds,
    Changed = OffCode =/= OnCode,
    ct:pal("~s: opt=~p, generated code changed: ~p (~p -> ~p instructions)",
           [ModBase, PassOpts, Changed,
            instr_count(OffCode), instr_count(OnCode)]),
    Changed.

instr_count(Code) ->
    lists:sum([length(Is) || {function,_,_,_,Is} <- Code]).

name_text(Name, Arity) ->
    atom_to_list(Name) ++ "/" ++ integer_to_list(Arity).

speedup_text(OffUs, OnUs) ->
    case OnUs of
        0 -> "inf";
        _ -> float_to_list(OffUs / OnUs, [{decimals,3}])
    end.

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
    Sub = "beam_ssa_opt_equiv_" ++
        integer_to_list(erlang:unique_integer([positive])),
    Dir = filename:join(Base, Sub),
    ok = filelib:ensure_path(Dir),
    Dir.

write_probe(Dir, Mod, Source) ->
    File = filename:join(Dir, Mod ++ ".erl"),
    ok = file:write_file(File, Source),
    File.
