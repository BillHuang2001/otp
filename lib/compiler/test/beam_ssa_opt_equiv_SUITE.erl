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
%% passes (beam_ssa_loop_opt / beam_ssa_loop_arith / beam_ssa_fus /
%% beam_ssa_fbip), plus a small self-contained benchmark of the same in-tree
%% compiler.
%%
%% THE GATE
%% The gate compiles the SAME probe module twice - once with the pass(es) OFF
%% (the base option list) and once with them ON - and then calls every probe
%% function with a battery of arguments in both builds. Every build must return
%% exactly the same `{ok,Value}' / `{Class,Reason}' results as the OFF build,
%% which is the semantics reference (the stacktrace is deliberately NOT part of
%% the comparison). This is the safety net the rewrite plan mandates: an opt-in
%% pass may not change observable behaviour, only the generated code.
%%
%% The passes are DEFAULT-OFF (see beam_ssa_opt:expand_opt/2), so for each case
%% the "OFF" build is `[binary,no_spawn_compiler_process]' and the "ON" build
%% adds the pass flag(s). `no_spawn_compiler_process' keeps the compilation in
%% the test process.
%%
%% Besides the behaviour, the suite reports WHICH functions a pass rewrote (the
%% OFF and ON disassemblies are compared function by function), so the log
%% shows that a case is not vacuous - and, just as importantly, that the cases
%% that must be left alone were left alone. Covered here:
%%
%%   * `loop_opt' (beam_ssa_loop_reduce): `X * 2' -> `X + X' for every number
%%     (floats and subnormals included), `X * 2^n' -> `X bsl n' only for a
%%     bounded operand and n =< 64, `X div 2^n' -> `X bsr n' and
%%     `X rem 2^n' -> `X band (2^n - 1)' only for a proven non-negative
%%     integer; the refusals (unbounded operand, negative operand, a huge
%%     literal shift count, `div'/`rem' by zero, an unproven operand) too.
%%   * `loop_arith_licm' (beam_ssa_loop_arith): an invariant `A + N' IS hoisted
%%     out of a receive loop, while `A bsl N' inside the same shape is NOT
%%     (a hoist would raise `system_limit' on paths the original never took).
%%   * `fusion_opt' (beam_ssa_fus): Fusion 5, the `has_map_field' +
%%     `maps:get/2' redundancy, plus the shapes it must not touch.
%%   * `fbip_opt' (beam_ssa_fbip): in-place `update_record' on a provably
%%     fresh, uniquely owned source, and the shared/aliased sources that must
%%     stay copies.
%%
%% THE BENCHMARK
%% The benchmark compiles a small hand-written kernel probe with all three
%% passes OFF vs ON, checks that both builds behave identically (reusing the
%% equivalence machinery), and measures compilation time plus the execution
%% time of each kernel. The kernels are sampled ?BENCH_REPS times per build,
%% interleaved: one repetition loads one build, discards a warm-up run of every
%% kernel and then times one run of every kernel, and the OFF and ON
%% repetitions are taken back to back (so a drift in the machine's load hits
%% both builds alike). The MEDIAN of the samples is reported together with the
%% interquartile range over the median, a robust spread: a single in-process
%% timing sample of these kernels has a 25-35% spread, which is wider than any
%% effect the opt-in passes have on them.
%%
%% It hard-asserts only that the ON build is not catastrophically slower
%% (`ON median =< OFF median * 2'). Whether the passes actually make a kernel
%% faster is REPORTED, never asserted, because the opt-in passes may
%% legitimately not change a given kernel at all; and the reported verdict is
%% qualified by the measured spread - a difference smaller than the spread of
%% the very same run is reported as "inconclusive" rather than as
%% "faster"/"slower". Do not read a verdict from a single run.
%%
-module(beam_ssa_opt_equiv_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([loop_opt_equivalence/1,
         loop_reduce_equivalence/1,
         loop_arith_licm_equivalence/1,
         fusion_opt_equivalence/1,
         fusion5_equivalence/1,
         fbip_opt_equivalence/1,
         all_passes_equivalence/1,
         benchmark/1]).

%% Samples taken per kernel per build by the benchmark.
-define(BENCH_REPS, 11).

suite() ->
    [{ct_hooks,[ts_install_cth]},
     {timetrap,{minutes,5}}].

all() ->
    [{group,p}].

groups() ->
    [{p,[],
      [loop_opt_equivalence,
       loop_reduce_equivalence,
       loop_arith_licm_equivalence,
       fusion_opt_equivalence,
       fusion5_equivalence,
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

%% The loop pass (beam_ssa_loop_opt) individually, ON vs OFF, on the broad
%% probe (a receive loop, map/record/arithmetic shapes, raisers).
loop_opt_equivalence(Config) ->
    _ = equivalence_case(Config, "beam_ssa_opt_equiv_loop", [loop_opt]),
    ok.

%% The whole-function strength reduction of beam_ssa_loop_reduce, ON vs OFF.
%% Every rewrite and every refusal has its own function in the probe, so the
%% per-function code-change report says exactly which ones fired.
loop_reduce_equivalence(Config) ->
    ModBase = "beam_ssa_opt_equiv_reduce",
    Changed = equivalence_case(Config, ModBase, reduce_probe(ModBase),
                               [loop_opt], reduce_cases()),
    %% Positive control: the strength reducer reduced at least one function of
    %% this probe, so the comparisons above are not vacuous. (`mul2/1' is the
    %% one rewrite that needs no proof at all and must always fire.)
    ?assert(lists:member({mul2,1}, Changed), Changed),
    ?assert(lists:member({mul32bounded,1}, Changed), Changed),
    ?assert(lists:member({divnat,1}, Changed), Changed),
    ?assert(lists:member({remnat,1}, Changed), Changed),
    %% ...and the refusals really are refusals.
    ?assertNot(lists:member({mul32,1}, Changed), Changed),
    ?assertNot(lists:member({mul_shift100,1}, Changed), Changed),
    ?assertNot(lists:member({divneg,1}, Changed), Changed),
    ?assertNot(lists:member({remneg,1}, Changed), Changed),
    ?assertNot(lists:member({divzero,1}, Changed), Changed),
    ?assertNot(lists:member({remzero,1}, Changed), Changed),
    ok.

%% The arithmetic LICM (beam_ssa_loop_arith, flag loop_arith_licm), ON vs OFF.
loop_arith_licm_equivalence(Config) ->
    ModBase = "beam_ssa_opt_equiv_licm",
    Changed = equivalence_case(Config, ModBase, licm_probe(ModBase),
                               [loop_opt,loop_arith_licm], licm_cases()),
    %% Positive control: the pass hoisted the invariant `A + N' out of the
    %% receive loop of arith_loop/3, so the pass ran at all in this build.
    ?assert(lists:member({arith_loop,3}, Changed), Changed),
    %% The `A bsl N' / `A bsr N' loops must be left exactly as they are:
    %% hoisting a shift would evaluate it on paths the original never took.
    ?assertNot(lists:member({shift_loop,3}, Changed), Changed),
    ?assertNot(lists:member({bsr_loop,3}, Changed), Changed),
    ?assertNot(lists:member({shift_any,3}, Changed), Changed),
    ok.

%% The map-lookup fusion pass (beam_ssa_fus) individually, ON vs OFF.
fusion_opt_equivalence(Config) ->
    _ = equivalence_case(Config, "beam_ssa_opt_equiv_fusion", [fusion_opt]),
    ok.

%% Fusion 5 (`has_map_field' + `maps:get/2' -> one `get_map_element'), ON vs
%% OFF, including the paths that must not be fused.
fusion5_equivalence(Config) ->
    ModBase = "beam_ssa_opt_equiv_fusion5",
    Changed = equivalence_case(Config, ModBase, fusion5_probe(ModBase),
                               [fusion_opt], fusion5_cases()),
    %% Positive control: at least one of the `maps:get/2' shapes was fused.
    ?assert(lists:member({get2,2}, Changed), Changed),
    %% A get without a preceding `has_map_field' test must keep raising.
    ?assertNot(lists:member({get2_noguard,2}, Changed), Changed),
    ok.

%% The FBIP pass (beam_ssa_fbip) individually, ON vs OFF.
fbip_opt_equivalence(Config) ->
    ModBase = "beam_ssa_opt_equiv_fbip",
    Changed = equivalence_case(Config, ModBase, [fbip_opt]),
    %% Positive control: the update of the freshly allocated record of
    %% rec_fresh/1 was turned into an in-place update, so the comparisons above
    %% are not vacuous.
    ?assert(lists:member({rec_fresh,1}, Changed), Changed),
    %% Negative control: every source that is still reachable (read after the
    %% update, stored in the process dictionary, captured by a fun) was left a
    %% copy, so the grid above also covers the pass declining.
    ?assertNot(lists:member({rec_keep,1}, Changed), Changed),
    ?assertNot(lists:member({rec_escape,1}, Changed), Changed),
    ?assertNot(lists:member({rec_fun,1}, Changed), Changed),
    ok.

%% All three opt-in passes together, ON vs OFF.
all_passes_equivalence(Config) ->
    _ = equivalence_case(Config, "beam_ssa_opt_equiv_all", passes()),
    ok.

%% Compile the kernel probe with all three passes OFF and ON, check that both
%% builds behave identically, and measure compilation time as well as the
%% execution time of every kernel. Reports the per-kernel median with its
%% spread, how many kernels have a lower median with the passes ON, and a
%% verdict that is qualified by the measured spread; hard-asserts only that the
%% ON build is not catastrophically slower than the OFF build.
benchmark(Config) ->
    ModBase = "beam_ssa_opt_bench",
    Dir = tmp_dir(Config),
    File = write_probe(Dir, ModBase, bench_probe(ModBase)),

    {OffMs, {OffMod, OffBin}} = compile_timed(File, base_opts()),
    {OnMs, {OnMod, OnBin}} = compile_timed(File, base_opts() ++ passes()),

    %% (i) The two builds must return exactly the same results/exceptions.
    check_battery([{off, OffMod, OffBin}, {on, OnMod, OnBin}],
                  bench_cases()),

    %% (ii) Measure the kernels: interleaved samples, one guard per kernel
    %% against a pathological regression (a loose `ON =< OFF * 2' bound on the
    %% medians).
    Samples = interleaved_samples(OffMod, OffBin, OnMod, OnBin,
                                  bench_kernels(), ?BENCH_REPS),
    Rows = [kernel_row(K, Samples) || K <- bench_kernels()],

    OffCode = disasm(OffBin),
    OnCode = disasm(OnBin),
    ChangedFuns = changed_funs(OffCode, OnCode),
    OffTot = lists:sum([median(Off) || {_, _, Off, _} <- Rows]),
    OnTot = lists:sum([median(On) || {_, _, _, On} <- Rows]),
    %% The uncertainty of the aggregate: the per-kernel spreads propagated to
    %% the sum of the medians, i.e. the relative spreads weighted by each
    %% kernel's share of the total. Taking the largest spread instead would be
    %% an upper bound dominated by the shortest, noisiest kernel.
    Band = lists:sum([median(Off) * spread(Off) + median(On) * spread(On)
                      || {_, _, Off, On} <- Rows]) / (OffTot + OnTot),
    RelDiff = (OnTot - OffTot) / OffTot,
    FasterCount = length([ok || {_, _, Off, On} <- Rows, median(On) < median(Off)]),
    KernelCount = length(Rows),
    Verdict =
        case abs(RelDiff) =< Band of
            true -> "inconclusive (the difference is inside the measured spread)";
            false when RelDiff < 0 -> "ON faster";
            false -> "ON slower"
        end,

    ct:pal("~n=== beam_ssa_opt benchmark (~s) ===", [ModBase]),
    ct:pal("method: ~w interleaved repetitions, see the module comment "
           "(the reported spread is the interquartile range over the median)",
           [?BENCH_REPS]),
    ct:pal("passes changed generated code: ~p (~p -> ~p instructions)",
           [ChangedFuns =/= [], instr_count(OffCode), instr_count(OnCode)]),
    case ChangedFuns of
        [] -> ok;
        _ -> ct:pal("    rewritten functions: ~s", [funs_text(ChangedFuns)])
    end,
    ct:pal("compilation time (min of 3)  OFF: ~.2f ms   ON: ~.2f ms",
           [OffMs, OnMs]),
    ct:pal("~-13s ~-22s ~-22s ~8s ~8s ~8s",
           ["kernel", "OFF min/med/max ms", "ON min/med/max ms", "ratio",
            "OFF sp", "ON sp"]),
    lists:foreach(fun({Name, Arity, Off, On}) ->
                          ct:pal("~-13s ~-22s ~-22s ~8.3f ~7.1f% ~7.1f%",
                                 [name_text(Name, Arity), triple_text(Off),
                                  triple_text(On),
                                  median(Off) / median(On),
                                  100 * spread(Off), 100 * spread(On)])
                  end, Rows),
    ct:pal("sum of medians   OFF: ~.3f ms   ON: ~.3f ms   ratio ON/OFF: ~.3f",
           [OffTot / 1000, OnTot / 1000, OnTot / OffTot]),
    ct:pal("kernels with a lower ON median: ~w of ~w",
           [FasterCount, KernelCount]),
    ct:pal("uncertainty of the aggregate: ~.1f% (the per-kernel spreads "
           "weighted by each kernel's share of the total)", [100 * Band]),
    ct:pal("VERDICT: ~s; ON/OFF median ratio ~.3f (~s), ~s the uncertainty "
           "(~.1f%) - a statement about THIS run only, never asserted.",
           [Verdict, OnTot / OffTot, pct_text(100 * RelDiff),
            case abs(RelDiff) =< Band of
                true -> "inside";
                false -> "outside"
            end,
            100 * Band]),
    ok.

%% Compile the equivalence probe with the pass options `PassOpts' OFF and ON
%% and check that both builds behave identically. Returns the sorted list of
%% `{Fun,Arity}' whose generated code the pass changed (which is also reported
%% to the log).
equivalence_case(Config, ModBase, PassOpts) ->
    equivalence_case(Config, ModBase, equiv_probe(ModBase), PassOpts,
                     equiv_cases()).

equivalence_case(Config, ModBase, Source, PassOpts, Cases) ->
    Sets = [{off, base_opts()},
            {on, base_opts() ++ PassOpts}],
    Builds = check_equivalence(Config, ModBase, Source, Sets, Cases),
    report_change(ModBase, PassOpts, Builds).

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
%% whole battery `Cases' (see `check_battery/2').
%%
%% Returns the builds as `{Label,Mod,Bin,Code}' (Code being the beam_disasm
%% function list), in the same order as `OptionSets'.
check_equivalence(Config, ProbeSourceBaseName, ProbeSource, OptionSets, Cases) ->
    Dir = tmp_dir(Config),
    File = write_probe(Dir, ProbeSourceBaseName, ProbeSource),
    Builds = [{Label, build(File, Opts)} || {Label,Opts} <- OptionSets],
    check_battery([{Label, Mod, Bin} || {Label,{Mod,Bin}} <- Builds], Cases),
    [{Label, Mod, Bin, disasm(Bin)} || {Label,{Mod,Bin}} <- Builds].

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

%% A module exercising every construct the opt-in passes look for: a receive
%% loop, a map-lookup guard with extraction, record construction/update, a
%% local freshly-allocating function whose result is updated (the FBIP
%% shape), record updates whose source is still aliased (where FBIP must keep
%% the copy), a list comprehension, integer arithmetic loops with `* 2', and
%% two functions that raise (a `badarith' and a `function_clause').
equiv_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([recv/1,mp/1,rec/1,rec_fresh/1,rec_loop/1,mk_fresh/1,\n",
     "         rec_keep/1,rec_escape/1,rec_fun/1,\n",
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
     "%% A record update whose source is read again afterwards: the update must\n",
     "%% stay a copy, so the older reference keeps the old field. An in-place\n",
     "%% update here would show up as a difference in `R0#r.b'.\n",
     "rec_keep(X) ->\n",
     "    R0 = #r{a = X, b = 0},\n",
     "    R1 = R0#r{b = X + 1},\n",
     "    {R1#r.a, R1#r.b, R0#r.b}.\n",
     "\n",
     "%% ... the same, with the older reference escaping into the process\n",
     "%% dictionary (the compiler cannot see through put/1).\n",
     "rec_escape(X) ->\n",
     "    R0 = #r{a = X, b = 0},\n",
     "    put(equiv_probe_alias, R0),\n",
     "    R1 = R0#r{b = X + 1},\n",
     "    Stored = get(equiv_probe_alias),\n",
     "    erase(equiv_probe_alias),\n",
     "    {R1#r.a, R1#r.b, Stored#r.a, Stored#r.b}.\n",
     "\n",
     "%% ... and with the older reference captured by a fun.\n",
     "rec_fun(X) ->\n",
     "    R0 = #r{a = X, b = 0},\n",
     "    F = fun() -> R0#r.b end,\n",
     "    R1 = R0#r{b = X + 1},\n",
     "    {R1#r.a, R1#r.b, F()}.\n",
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
     {rec_keep, [[1], [42], [-7], [x]]},
     {rec_escape, [[1], [42], [-7], [x]]},
     {rec_fun, [[1], [42], [-7], [x]]},
     {lc, [[[1, 2, 3]], [[]], ["abc"], [[a, b]]]},
     {arith, [[0], [5], [-3], [x]]},
     {sum, [[[1, 2, 3]], [[]], [[10, -3, 2]]]},
     {ex, [[2], [0], [x]]},
     {fc, [[a], [b]]}].

%% One function per rewrite (or refusal) of `beam_ssa_loop_reduce', so that the
%% per-function code-change report says exactly which of them fired; the
%% reducer works on the whole function, so no loop is needed. The rules are
%% listed in the module comment:
%%
%%   mul2/1 -> X + X   mul32bounded/1 -> X bsl 5   divnat/1 -> X bsr 2
%%   remnat/1 -> X band 7
%%
%% and mul32/1, mul_shift100/1, divneg/1, remneg/1, divany/1, remany/1,
%% divzero/1, remzero/1 are the refusals (litshift/0 is folded by the front
%% end, not by this pass).
reduce_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([mul2/1,mul32/1,mul32bounded/1,mul_shift100/1,divnat/1,divneg/1,\n",
     "         remnat/1,remneg/1,divany/1,remany/1,divzero/1,remzero/1,\n",
     "         litshift/0]).\n",
     "\n",
     "mul2(X) -> X * 2.\n",
     "\n",
     "mul32(X) when is_integer(X) -> X * 32;\n",
     "mul32(_) -> not_int.\n",
     "\n",
     "mul32bounded(X) when is_integer(X), X >= 0, X < 1000 -> X * 32;\n",
     "mul32bounded(_) -> out_of_range.\n",
     "\n",
     "mul_shift100(X) when is_integer(X), X >= 0, X < 1000 -> X * (1 bsl 100);\n",
     "mul_shift100(_) -> out_of_range.\n",
     "\n",
     "divnat(N) when is_integer(N), N >= 0 -> N div 4;\n",
     "divnat(_) -> not_nat.\n",
     "\n",
     "divneg(N) when is_integer(N) -> N div 4;\n",
     "divneg(_) -> not_int.\n",
     "\n",
     "remnat(N) when is_integer(N), N >= 0 -> N rem 8;\n",
     "remnat(_) -> not_nat.\n",
     "\n",
     "remneg(N) when is_integer(N) -> N rem 8;\n",
     "remneg(_) -> not_int.\n",
     "\n",
     "divany(N) -> N div 4.\n",
     "\n",
     "remany(N) -> N rem 8.\n",
     "\n",
     "divzero(N) -> N div 0.\n",
     "\n",
     "remzero(N) -> N rem 0.\n",
     "\n",
     "litshift() -> (1 bsl 200) * 32.\n"].

%% Argument sets for the strength-reduction probe.
%%
%% `mul2/1' is fed floats, subnormals and non-numbers (it must double a number
%% exactly and raise `badarith' otherwise), `mul32/1' is fed bignums far past
%% the range the type analysis can bound (the exact product must still come
%% back), and the `div'/`rem' probes are fed negative and non-integer operands
%% (which must keep `div'/'rem' semantics).
reduce_cases() ->
    [{mul2, [[0], [1], [-1], [3], [2.5], [-0.5], [-0.0], [1.0e308],
             [5.0e-324], [1.0e-320], [1 bsl 200], [-(1 bsl 200)],
             [not_a_number], [[1]]]},
     {mul32, [[0], [1], [-3], [7], [1 bsl 200], [-(1 bsl 200)],
              [1 bsl 3920000], [1.5], [not_a_number]]},
     {mul32bounded, [[0], [1], [999], [1000], [-1], [1 bsl 200], [1.5],
                     [not_a_number]]},
     {mul_shift100, [[0], [1], [999], [1000], [-1], [not_a_number]]},
     {divnat, [[0], [1], [3], [7], [8], [12], [1 bsl 200], [-1], [-12],
               [8.0], [not_a_number]]},
     {divneg, [[0], [1], [-1], [-3], [-7], [-8], [12], [1 bsl 200],
               [-(1 bsl 200)], [8.0], [not_a_number]]},
     {remnat, [[0], [1], [7], [8], [15], [1 bsl 200], [-1], [8.0],
               [not_a_number]]},
     {remneg, [[0], [-1], [-7], [-8], [9], [-(1 bsl 200)], [8.0],
               [not_a_number]]},
     {divany, [[7], [-7], [8.0], [not_a_number]]},
     {remany, [[7], [-7], [8.0], [not_a_number]]},
     {divzero, [[1], [1.0], [not_a_number]]},
     {remzero, [[1], [1.0], [not_a_number]]},
     {litshift, [[]]}].

%% Receive loops whose bodies use a loop-invariant `A + N' (hoistable) and a
%% loop-invariant `A bsl N' (NOT hoistable: `bsl' is not in the pass's
%% hoistable set, because a left shift raises `system_limit' past the bignum
%% arity bound and hoisting it into the preheader would raise it on paths the
%% original never took).
%%
%% The `_loop/N' functions are private, so the type of their arguments is the
%% one the exported guarded wrapper proved; they are reached through the
%% exported wrappers, which the gate calls. `*_any*' has no guard at all, so
%% the operands are unproven and nothing may be hoisted.
licm_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([arith_loop/2,arith_go/2,shift_loop/2,shift_go/2,\n",
     "         bsr_loop/2,bsr_go/2,\n",
     "         arith_any_go/2,shift_any_go/2]).\n",
     "\n",
     "%% Invariant `A + N' inside the loop: hoisted by loop_arith_licm.\n",
     "arith_loop(A, N) when is_integer(A), is_integer(N) -> arith_loop(A, N, 0).\n",
     "arith_loop(A, N, Acc) ->\n",
     "    receive\n",
     "        {go, X} -> arith_loop(A, N, Acc + (A + N) + X)\n",
     "    after 0 -> Acc\n",
     "    end.\n",
     "\n",
     "%% The same loop with a message waiting, so the arithmetic is evaluated.\n",
     "arith_go(A, N) when is_integer(A), is_integer(N) ->\n",
     "    self() ! {go, 1},\n",
     "    arith_loop(A, N, 0).\n",
     "\n",
     "%% Invariant `A bsl N' inside the loop: must NOT be hoisted. With an empty\n",
     "%% mailbox the loop never evaluates it, so a huge shift count must simply\n",
     "%% return the accumulator.\n",
     "shift_loop(A, N) when is_integer(A), is_integer(N) -> shift_loop(A, N, 0).\n",
     "shift_loop(A, N, Acc) ->\n",
     "    receive\n",
     "        {go, X} -> shift_loop(A, N, Acc + (A bsl N) + X)\n",
     "    after 0 -> Acc\n",
     "    end.\n",
     "\n",
     "%% The same loop with a message waiting: the shift IS evaluated, so a huge\n",
     "%% count (and a non-integer operand) must raise exactly as it does without\n",
     "%% the pass.\n",
     "shift_go(A, N) when is_integer(A), is_integer(N) ->\n",
     "    self() ! {go, 1},\n",
     "    shift_loop(A, N, 0).\n",
     "\n",
     "%% The same shape with `A bsr N', the other shift the LICM pass refuses to
%% hoist for the same reason (`A bsr N' with a negative N is a left shift, so a
%% huge count raises `system_limit' just like `bsl').
bsr_loop(A, N) when is_integer(A), is_integer(N) -> bsr_loop(A, N, 0).
bsr_loop(A, N, Acc) ->
    receive
        {go, X} -> bsr_loop(A, N, Acc + (A bsr N) + X)
    after 0 -> Acc
    end.

bsr_go(A, N) when is_integer(A), is_integer(N) ->
    self() ! {go, 1},
    bsr_loop(A, N, 0).

%% No integrality proof at all: nothing may be hoisted.\n",
     "arith_any_go(A, N) ->\n",
     "    self() ! {go, 1},\n",
     "    arith_any(A, N, 0).\n",
     "arith_any(A, N, Acc) ->\n",
     "    receive\n",
     "        {go, X} -> arith_any(A, N, Acc + (A + N) + X)\n",
     "    after 0 -> Acc\n",
     "    end.\n",
     "\n",
     "shift_any_go(A, N) ->\n",
     "    self() ! {go, 1},\n",
     "    shift_any(A, N, 0).\n",
     "shift_any(A, N, Acc) ->\n",
     "    receive\n",
     "        {go, X} -> shift_any(A, N, Acc + (A bsl N) + X)\n",
     "    after 0 -> Acc\n",
     "    end.\n"].

%% Argument sets for the LICM probe. The `receive ... after 0' runs in the
%% fresh process `run/4' spawns, so its mailbox is empty unless the function
%% sends itself a message first.
licm_cases() ->
    [{arith_loop, [[0, 0], [3, 4], [-5, 2], [-5, -2], [1 bsl 200, 1],
                   [1.5, 2], [not_a_number, 1], [1, not_a_number]]},
     {arith_go, [[0, 0], [3, 4], [-5, 2], [1 bsl 200, 1], [1 bsl 200, -(1 bsl 200)],
                 [1.5, 2], [not_a_number, 1]]},
     %% `shift_loop/2' with an empty mailbox returns the accumulator; the huge
     %% counts would raise `system_limit' if the shift were hoisted.
     {shift_loop, [[0, 0], [1, 3], [255, 64], [1, 5000000],
                   [1, -5000000], [-1, -5000000], [1, 0],
                   [1.5, 3], [1, 3.0], [not_a_number, 3]]},
     {shift_go, [[1, 3], [255, 8], [1, 0], [1, -1], [0, 5000000],
                 [1, 5000000], [1, -5000000], [1.5, 3]]},
     %% `bsr_loop/2' likewise: with an empty mailbox the shift is never
     %% evaluated, and a negative count is a huge left shift that would raise
     %% `system_limit' if the hoist were allowed.
     {bsr_loop, [[0, 0], [255, 3], [1, 0], [1, -5000000], [-1, -5000000],
                 [8, 2], [1.5, 3], [1, 3.0], [not_a_number, 3]]},
     {bsr_go, [[255, 3], [-8, 2], [1, 0], [1, -5000000], [0, 5000000],
               [2.5, 3], [1, 3.0], [not_a_number, 3]]},
     {arith_any_go, [[2.5, 1], [3, 4], [not_a_number, 1], [1, not_a_number]]},
     {shift_any_go, [[1, 3], [2.5, 3], [1, 3.0], [1, -1], [1, 5000000],
                     [not_a_number, 3]]}].

%% `has_map_field(Map, Key)' immediately followed by `maps:get(Key, Map)' on
%% its success edge, which is one redundant map probe that Fusion 5 (commit
%% 3464e74a26) collapses into a single `get_map_element'. Plus the shapes that
%% must NOT be fused: a `maps:get/2' without a preceding test (which must keep
%% raising `{badkey,_}'/`{badmap,_}'), a two-key chain, and an extraction that
%% is used twice.
fusion5_probe(Mod) ->
    ["-module(", Mod, ").\n",
     "-export([get2/2,get2_chain/3,get2_noguard/2,get2_const/1,get2_used/2]).\n",
     "\n",
     "%% The Fusion 5 shape. Note that the guard already proves the key to be\n",
     "%% present, so the fusion cannot change which of the two paths is taken.\n",
     "get2(M, K) when is_map(M), is_map_key(K, M) -> maps:get(K, M);\n",
     "get2(_, _) -> no_key.\n",
     "\n",
     "%% Two keys, both tested and both fetched.\n",
     "get2_chain(M, K1, K2) when is_map(M), is_map_key(K1, M), is_map_key(K2, M) ->\n",
     "    maps:get(K1, M) + maps:get(K2, M);\n",
     "get2_chain(_, _, _) -> no_key.\n",
     "\n",
     "%% No `is_map_key/2' test: `maps:get/2' must keep raising.\n",
     "get2_noguard(M, K) -> maps:get(K, M).\n",
     "\n",
     "%% A constant key under an `is_map/1' + `is_map_key/2' guard, plus a\n",
     "%% defaulted lookup that must not be touched.\n",
     "get2_const(M) when is_map(M), is_map_key(a, M) ->\n",
     "    {maps:get(a, M), maps:get(b, M, 0)};\n",
     "get2_const(_) -> no_key.\n",
     "\n",
     "%% The extracted value is used more than once.\n",
     "get2_used(M, K) when is_map(M), is_map_key(K, M) ->\n",
     "    V = maps:get(K, M),\n",
     "    {V, V + 1};\n",
     "get2_used(_, _) -> {no_key, no_key}.\n"].

%% Argument sets for the Fusion 5 probe. `get2/2' must take the same branch
%% ON and OFF for a present key, a missing key (the guard fails, so the
%% branch is taken and `maps:get/2' is never reached) and a non-map operand;
%% `get2_noguard/2' pins the `{badkey,_}' and `{badmap,_}' exceptions of the
%% un-fused `maps:get/2'.
fusion5_cases() ->
    [{get2, [[#{a => 1}, a], [#{a => 1, b => 2}, b], [#{a => 1}, b],
             [#{}, a], [not_a_map, a], [[{a, 1}], a], [5, a]]},
     {get2_chain, [[#{a => 1, b => 2}, a, b], [#{a => 1}, a, b],
                   [#{a => 1, b => 2}, a, c], [#{}, a, b],
                   [not_a_map, a, b]]},
     {get2_noguard, [[#{a => 1}, a], [#{a => 1}, b], [#{}, a], [[], a],
                     [5, a], [not_a_map, k]]},
     {get2_const, [[#{a => 1}], [#{a => 1, b => 9}], [#{b => 9}], [#{}],
                   [[{a, 1}]], [5]]},
     {get2_used, [[#{a => 1}, a], [#{a => 1}, b], [#{}, a],
                  [not_a_map, a]]}].

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

%% {Fun, Args}: each kernel is sampled ?BENCH_REPS times per build
%% (interleaved, see `interleaved_samples/6') and the median is reported.
bench_kernels() ->
    [{sum, [lists:seq(1, 400000)]},
     {count, [1500000]},
     {tak, [23, 12, 6]},
     {fib, [28]},
     {maplookup, [80000]},
     {rec_upd, [1500000]},
     {arith, [800000]}].

%% The small argument sets used for the behavioural check of the kernel probe.
bench_cases() ->
    [{sum, [[1, 2, 3, 4, 5]]},
     {count, [[100]]},
     {tak, [[10, 5, 2]]},
     {fib, [[10]]},
     {maplookup, [[100]]},
     {rec_upd, [[100]]},
     {arith, [[100]]}].

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

%% `Reps' interleaved repetitions over ALL kernels: one repetition times the
%% kernels of the OFF build and then the kernels of the ON build, so a drift in
%% the machine's load hits both builds alike; each build is loaded only once
%% per repetition, and every kernel is warmed up before it is timed.
%% Returns `#{{Fun,Arity} => {[OffUs], [OnUs]}}'.
interleaved_samples(OffMod, OffBin, OnMod, OnBin, Kernels, Reps) ->
    lists:foldl(
      fun(_Rep, Acc0) ->
              OffTimes = time_build(OffMod, OffBin, Kernels),
              OnTimes = time_build(OnMod, OnBin, Kernels),
              lists:foldl(
                fun({{Fun, Args}, {OffUs, OnUs}}, Acc) ->
                        Key = {Fun, length(Args)},
                        {Offs, Ons} = maps:get(Key, Acc, {[], []}),
                        Acc#{Key => {[OffUs | Offs], [OnUs | Ons]}}
                end, Acc0, lists:zip(Kernels, lists:zip(OffTimes, OnTimes)))
      end, #{}, lists:seq(1, Reps)).

%% One pass over all kernels in one build: load the build, discard one
%% warm-up run of every kernel (so that the reload and the first execution of
%% each kernel cannot show up in the number), then time one run of every
%% kernel. Returns the times in microseconds, in the order of `Kernels'.
time_build(Mod, Bin, Kernels) ->
    load(Mod, Bin),
    lists:foreach(fun({Fun, Args}) -> _ = apply(Mod, Fun, Args) end, Kernels),
    [begin
         {Us, _Result} = timer:tc(fun() -> apply(Mod, Fun, Args) end),
         Us
     end || {Fun, Args} <- Kernels].

%% Turn the samples of one kernel into a reported row, guarding against a
%% catastrophic regression: the ON median must not be more than twice the OFF
%% median. Returns `{Fun, Arity, OffStats, OnStats}' with Stats as in
%% `stats/1'.
kernel_row({Fun, Args}, Samples) ->
    Arity = length(Args),
    {OffUs, OnUs} = maps:get({Fun, Arity}, Samples),
    OffStats = stats(OffUs),
    OnStats = stats(OnUs),
    case median(OnStats) =< median(OffStats) * 2 of
        true ->
            ok;
        false ->
            ct:fail({kernel_median_slower_with_passes, {Fun, Arity},
                     {off, OffStats}, {on, OnStats}})
    end,
    {Fun, Arity, OffStats, OnStats}.

%% Timing statistics of one kernel's samples, in microseconds. The MEDIAN is
%% the reported value and `spread' is the interquartile range over it - a
%% robust relative spread (min and max are still reported, so outliers remain
%% visible).
stats(Us) ->
    Sorted = lists:sort(Us),
    N = length(Sorted),
    Med = lists:nth((N + 1) div 2, Sorted),
    Q1 = lists:nth((N + 3) div 4, Sorted),
    Q3 = lists:nth((3 * N + 3) div 4, Sorted),
    #{min => hd(Sorted),
      med => Med,
      max => lists:last(Sorted),
      spread => case Med > 0 of
                    true -> (Q3 - Q1) / Med;
                    false -> 0.0
                end}.

median(Stats) -> maps:get(med, Stats).

spread(Stats) -> maps:get(spread, Stats).

%% Compile `File' with `Opts' three times and return the minimum compile time
%% in milliseconds together with one build. Repeating removes the first-compile
%% warm-up from the reported number.
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

%% The `{Function,Arity}'s whose generated code differs between the two
%% builds, i.e. the functions the option actually rewrote.
%%
%% The labels are renumbered by first appearance before comparing: `beam_asm'
%% takes label numbers from a module-global counter, so a pass that drops a
%% block anywhere in the module shifts the labels of every function after it.
%% Without the renumbering almost every function would look rewritten.
changed_funs(OffCode, OnCode) ->
    Off = maps:from_list([{{N,A},canon_labels(Is)} || {function,N,A,_,Is} <- OffCode]),
    On = maps:from_list([{{N,A},canon_labels(Is)} || {function,N,A,_,Is} <- OnCode]),
    lists:sort([FA || {FA, Is} <- maps:to_list(Off),
                      maps:get(FA, On, undefined) =/= Is]).

%% Renumber the labels of one function by order of first appearance, so that
%% two builds of the same function are compared by structure and not by the
%% absolute label numbers.
canon_labels(Is) ->
    {_Next, Map} =
        lists:foldl(fun({label, N}, {Next, M}) ->
                            case is_map_key(N, M) of
                                true -> {Next, M};
                                false -> {Next + 1, M#{N => Next}}
                            end;
                       (_I, Acc) ->
                            Acc
                    end, {1, #{}}, Is),
    [canon_instr(I, Map) || I <- Is].

canon_instr({label, N}, Map) ->
    {label, maps:get(N, Map, N)};
canon_instr({f, N}, Map) ->
    {f, maps:get(N, Map, N)};
canon_instr(I, Map) when is_tuple(I) ->
    list_to_tuple([canon_instr(E, Map) || E <- tuple_to_list(I)]);
canon_instr(I, Map) when is_list(I) ->
    [canon_instr(E, Map) || E <- I];
canon_instr(I, _Map) ->
    I.

%% Report (and return) which functions the option rewrote, which is what
%% shows that the pass fired - and, for the cases that must be left alone,
%% that it did not.
report_change(ModBase, PassOpts, Builds) ->
    [{off, _, _, OffCode}, {on, _, _, OnCode}] = Builds,
    Changed = changed_funs(OffCode, OnCode),
    ct:pal("~s: opt=~p, generated code changed: ~p (~p -> ~p instructions)",
           [ModBase, PassOpts, Changed =/= [],
            instr_count(OffCode), instr_count(OnCode)]),
    case Changed of
        [] -> ok;
        _ -> ct:pal("    rewritten functions: ~s", [funs_text(Changed)])
    end,
    Changed.

instr_count(Code) ->
    lists:sum([length(Is) || {function,_,_,_,Is} <- Code]).

funs_text([FA | FAs]) ->
    [name_text(FA) | [[", ", name_text(FA1)] || FA1 <- FAs]].

name_text({Name, Arity}) ->
    name_text(Name, Arity).

name_text(Name, Arity) ->
    atom_to_list(Name) ++ "/" ++ integer_to_list(Arity).

triple_text(Stats) ->
    io_lib:format("~.3f/~.3f/~.3f", [maps:get(min, Stats) / 1000,
                                     maps:get(med, Stats) / 1000,
                                     maps:get(max, Stats) / 1000]).

%% A signed percentage, e.g. "+3.6%" / "-2.1%".
pct_text(Percent) when Percent >= 0 ->
    io_lib:format("+~.1f%", [Percent]);
pct_text(Percent) ->
    io_lib:format("~.1f%", [Percent]).

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
