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
%% loop_opt_bench -- RUNTIME speed measurement for the opt-in `loop_opt' +
%% `loop_arith_licm' Stage-2 passes (see ../src/beam_ssa_loop_opt.erl).
%%
%% This is the execution-time counterpart of stage2_bench.erl, which measures
%% only COMPILE time.  It compiles a set of small but *realistic* tail-recursive
%% Erlang loops -- the shape every Erlang/Elixir/LFE loop lowers to -- twice with
%% THIS tree's compiler (built by lib/compiler/scripts/host_env.sh):
%%
%%     off   [binary, return]
%%     on    [binary, return, loop_opt, loop_arith_licm]
%%
%% loads both builds into ONE VM under two distinct module names, and times them
%% in the same process with interleaved paired samples.  Because the two builds
%% live side by side, VM warm-up, JIT state, code-server state and host load are
%% paid identically by both, and only the loop body differs.
%%
%% For kernels whose rewrites can be written by hand, a THIRD variant is built
%% from a hand-rewritten source compiled with the flags OFF ("manual": the same
%% loop with `band'/`bsr'/`+` spelled out).  It is the ceiling of what the
%% rewrite can possibly buy on this machine: if `manual' is not faster than
%% OFF, then nobody -- compiler pass or hand -- can win by rewriting these
%% operations here, whatever the pass does.
%%
%% What is measured, per kernel:
%%   * OFF median / ON median / ratio and the run's own relative spread (IQR
%%     over the median, per variant, combined conservatively), plus the MANUAL
%%     median and its ratio against OFF where a hand rewrite exists.
%%   * REACH: whether the generated `Code' chunk changed at all, plus the
%%     instruction-shape delta (how many `div'/`rem'/`*' bifs the OFF build
%%     issues versus how many `bsr'/`band'/`i_plus' the ON build issues).  A
%%     "speedup" measured on identical code is worthless, so the harness FAILS
%%     when a kernel that is declared to reach does not reach.
%%   * ALLOCATION: per-process GC count, words reclaimed and final heap size
%%     around one isolated run of each variant.  The strength reductions change
%%     CPU work, not allocation, so these are expected to be equal -- reporting
%%     them is what makes that claim checkable instead of assumed.
%%   * Optionally a whole-program cross-check: one kernel module is also run in
%%     a fresh `erl -noshell' process per variant, alternating rounds, median of
%%     the in-VM elapsed time (a second, cold-code noise model).
%%
%% Run it through test/measure_loop_opt_speed.sh; do not invoke it directly.
%% It needs (a) the host Erlang, (b) the rig compiler as the FIRST `compile' on
%% the code path -- it hard-fails when code:which(compile) is not the rig, and
%% (c) $SCRATCH (the wrapper's scratch dir) for the .beam files that
%% beam_disasm:file/1 needs.
%%
%% HONESTY NOTE: the host running this is shared and heavily loaded (see the
%% /proc/loadavg line in the output), so absolute times are NOT reproducible and
%% must never be quoted.  Only the ON/OFF ratio measured inside one session is a
%% signal, and even that is only meaningful together with the spread column and
%% the reach column.
%%
-module(loop_opt_bench).

-export([main/0, main/1]).

%% Measured repetitions of the ON/OFF pair per kernel (rotated order), on top
%% of one discarded warm-up round.  Forced odd so the median is a real sample.
-define(DEFAULT_REPS, 15).

%% Rounds of the whole-program (fresh VM per variant) cross-check.
-define(DEFAULT_ROUNDS, 5).

%% Iteration counts are tuned so one sample is a few tens of milliseconds on a
%% loaded host: long enough to dominate timer noise, short enough that the whole
%% sweep stays in the seconds range.
-define(ITERS_SHIFT_LCG, 16000000).
-define(ITERS_ADLER, 16000000).
-define(ITERS_QUANT, 12000000).
-define(ITERS_MUL2, 32000000).
-define(ITERS_BITPACK, 12000000).

%% Chunk id of the generated code in a BEAM file.
-define(CODE_CHUNK, <<"Code">>).

%% Instruction names (as reported by beam_disasm) that the loop pass removes:
%% the expensive integer arithmetic.  The disassembler reports every BIF as
%% {gc_bif,Op,...}, so instr_name/1 normalises that to {Op,'bif'}.
-define(EXPENSIVE_OPS, [{'div', 'bif'}, {'rem', 'bif'}, {'*', 'bif'}]).
%% ... and the cheap ones it introduces in their place: shifts and masks.
-define(CHEAP_OPS, [{'bsl', 'bif'}, {'bsr', 'bif'}, {'band', 'bif'}]).

-type variant() :: off | on | manual.
-type kernel() :: #{name := atom(),
                    src := string(),
                    manual => string(),
                    note := string(),
                    expect_reach := boolean(),
                    whole_program := boolean()}.
-type build() :: #{name := atom(),
                   note := string(),
                   expect_reach := boolean(),
                   whole_program := boolean(),
                   mods := #{variant() => atom()},
                   code := #{variant() => binary()},   %% Code chunk
                   ops := #{variant() => {non_neg_integer(), non_neg_integer()}},
                   results := #{variant() => term()}}.
-type timing() :: {atom(), #{variant() => [non_neg_integer()]}}.

%% ===================================================================
%% Entry point
%% ===================================================================

-spec main() -> no_return().
main() ->
    main(init:get_plain_arguments()).

-spec main([string()]) -> no_return().
main(Args) ->
    {Reps, Rounds} = parse_args(Args),
    ok = check_compiler(),
    Scratch = scratch_dir(),
    print_header(Reps, Rounds),
    Built = [build(K, Scratch) || K <- kernels()],
    ok = check_reach(Built),
    report_reach(Built),
    Timings = measure(Reps, Built),
    Alloc = measure_alloc(Built),
    report(Built, Timings, Alloc),
    Wp = whole_program(Built, Rounds),
    report_whole_program(Wp),
    %% halt/1 does not flush the io queue: without {flush,true} the table can be
    %% lost when stdout is a file or a pipe.
    erlang:halt(0, [{flush, true}]).

%% ===================================================================
%% Arguments and environment
%% ===================================================================

-spec parse_args([string()]) -> {pos_integer(), non_neg_integer()}.
parse_args([]) ->
    {?DEFAULT_REPS, ?DEFAULT_ROUNDS};
parse_args([Reps]) ->
    {to_int("repetitions", Reps, 11), ?DEFAULT_ROUNDS};
parse_args([Reps, Rounds]) ->
    {to_int("repetitions", Reps, 11), to_int("rounds", Rounds, 0)};
parse_args(Extra) ->
    die("too many arguments: " ++ string:join(Extra, " ")).

%% An odd count of repetitions makes the median an actually observed sample.
-spec to_int(string(), string(), non_neg_integer()) -> non_neg_integer().
to_int(_What, Str, Min) ->
    try list_to_integer(Str) of
        N when N >= Min, N rem 2 =:= 1 -> N;
        N when N >= Min -> N + 1;
        N -> die(Str ++ ": must be >= " ++ integer_to_list(Min)
                 ++ ", got " ++ integer_to_list(N))
    catch
        error:badarg -> die(Str ++ ": not an integer: " ++ Str)
    end.

%% The wrapper exports RIG_EBIN.  Refuse to run against any other compiler: an
%% escript or a missing -pa would silently measure the *host* OTP compiler,
%% whose default pipeline knows nothing about the Stage-2 flags, so every kernel
%% would be "unchanged" and the ratio would measure noise.
-spec check_compiler() -> ok.
check_compiler() ->
    case os:getenv("RIG_EBIN") of
        false ->
            die("RIG_EBIN is not set; run test/measure_loop_opt_speed.sh "
                "instead of loop_opt_bench directly (compile module is "
                ++ code:which(compile) ++ ")");
        Rig ->
            Which = code:which(compile),
            case lists:prefix(Rig, Which) of
                true ->
                    ok;
                false ->
                    die("wrong compiler: code:which(compile) = " ++ Which
                        ++ " is not under RIG_EBIN=" ++ Rig)
            end
    end.

-spec scratch_dir() -> string().   %% the dir holding the kernel .beam files
scratch_dir() ->
    case os:getenv("SCRATCH") of
        false -> die("SCRATCH is not set; run test/measure_loop_opt_speed.sh");
        Dir ->
            BeamDir = filename:join(Dir, "beam"),
            ok = filelib:ensure_dir(filename:join(BeamDir, "x")),
            BeamDir
    end.

%% ===================================================================
%% The kernels
%% ===================================================================

%% Every kernel is a standalone module exporting run/0, so both builds are
%% timed through the same entry point.  Each one is dominated by ONE family of
%% rewrites the loop pass owns, on the hot path, so a win -- or the absence of
%% one -- is attributable.
%%
%% `expect_reach' is the harness' hard assertion: false is only legal for a
%% deliberate control kernel, and it is reported as a documented null result.
-spec kernels() -> [kernel()].
kernels() ->
    [#{name => lcg_shift,
       note => "power-of-two LCG: rem 65536, div 256, rem 256, * 1024",
       expect_reach => true,
       whole_program => true,
       src => kernel_src(?ITERS_SHIFT_LCG,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 12345, 0).\n"
                         "\n"
                         "loop(0, _, A) ->\n"
                         "    A;\n"
                         "loop(N, S, A) ->\n"
                         "    S1 = (S * 1024 + 12345) rem 65536,\n"
                         "    A1 = A + (S1 div 256) + (S1 rem 256),\n"
                         "    loop(N - 1, S1, A1).\n"),
       manual => kernel_src(?ITERS_SHIFT_LCG,
                            "-export([run/0]).\n"
                            "\n"
                            "run() -> loop(?ITERS, 12345, 0).\n"
                            "\n"
                            "loop(0, _, A) ->\n"
                            "    A;\n"
                            "loop(N, S, A) ->\n"
                            "    S1 = ((S bsl 10) + 12345) band 65535,\n"
                            "    A1 = A + (S1 bsr 8) + (S1 band 255),\n"
                            "    loop(N - 1, S1, A1).\n")},

     #{name => adler_pow2,
       note => "Adler-style rolling checksum: two rem 65536 per iteration",
       expect_reach => true,
       whole_program => false,
       src => kernel_src(?ITERS_ADLER,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 1, 0, 0).\n"
                         "\n"
                         "loop(0, _, A, B) ->\n"
                         "    (B bsl 16) bor A;\n"
                         "loop(N, I, A, B) ->\n"
                         "    A1 = (A + I) rem 65536,\n"
                         "    B1 = (B + A1) rem 65536,\n"
                         "    loop(N - 1, I + 1, A1, B1).\n"),
       manual => kernel_src(?ITERS_ADLER,
                            "-export([run/0]).\n"
                            "\n"
                            "run() -> loop(?ITERS, 1, 0, 0).\n"
                            "\n"
                            "loop(0, _, A, B) ->\n"
                            "    (B bsl 16) bor A;\n"
                            "loop(N, I, A, B) ->\n"
                            "    A1 = (A + I) band 65535,\n"
                            "    B1 = (B + A1) band 65535,\n"
                            "    loop(N - 1, I + 1, A1, B1).\n")},

     #{name => fixed_point,
       note => "fixed-point quantisation: div 1024 / rem 1024 of a bounded V",
       expect_reach => true,
       whole_program => false,
       src => kernel_src(?ITERS_QUANT,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 0, 0).\n"
                         "\n"
                         "loop(0, _, Acc) ->\n"
                         "    Acc;\n"
                         "loop(N, S, Acc) ->\n"
                         "    V = S rem 1048576,\n"
                         "    Acc1 = Acc + (V div 1024) + (V rem 1024),\n"
                         "    loop(N - 1, S + 7919, Acc1).\n"),
       manual => kernel_src(?ITERS_QUANT,
                            "-export([run/0]).\n"
                            "\n"
                            "run() -> loop(?ITERS, 0, 0).\n"
                            "\n"
                            "loop(0, _, Acc) ->\n"
                            "    Acc;\n"
                            "loop(N, S, Acc) ->\n"
                            "    V = S band 1048575,\n"
                            "    Acc1 = Acc + (V bsr 10) + (V band 1023),\n"
                            "    loop(N - 1, S + 7919, Acc1).\n")},

     #{name => mul2_sum,
       note => "unbounded * 2 in an accumulation loop (the X+X rewrite)",
       expect_reach => true,
       whole_program => false,
       src => kernel_src(?ITERS_MUL2,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 1, 0).\n"
                         "\n"
                         "loop(0, _, Acc) ->\n"
                         "    Acc;\n"
                         "loop(N, I, Acc) ->\n"
                         "    loop(N - 1, I + 1, Acc + I * 2).\n"),
       manual => kernel_src(?ITERS_MUL2,
                            "-export([run/0]).\n"
                            "\n"
                            "run() -> loop(?ITERS, 1, 0).\n"
                            "\n"
                            "loop(0, _, Acc) ->\n"
                            "    Acc;\n"
                            "loop(N, I, Acc) ->\n"
                            "    loop(N - 1, I + 1, Acc + (I + I)).\n")},

     #{name => base64_digits,
       note => "base-64 digit extraction: div 64 / rem 64 / div 4096",
       expect_reach => true,
       whole_program => false,
       src => kernel_src(?ITERS_QUANT,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 123456789, 0).\n"
                         "\n"
                         "loop(0, _, Acc) ->\n"
                         "    Acc;\n"
                         "loop(N, V0, Acc) ->\n"
                         "    V = V0 rem 16777216,\n"
                         "    D0 = V rem 64,\n"
                         "    D1 = (V div 64) rem 64,\n"
                         "    D2 = (V div 4096) rem 64,\n"
                         "    loop(N - 1, V + 65599, Acc + D0 + D1 + D2).\n"),
       manual => kernel_src(?ITERS_QUANT,
                            "-export([run/0]).\n"
                            "\n"
                            "run() -> loop(?ITERS, 123456789, 0).\n"
                            "\n"
                            "loop(0, _, Acc) ->\n"
                            "    Acc;\n"
                            "loop(N, V0, Acc) ->\n"
                            "    V = V0 band 16777215,\n"
                            "    D0 = V band 63,\n"
                            "    D1 = (V bsr 6) band 63,\n"
                            "    D2 = (V bsr 12) band 63,\n"
                            "    loop(N - 1, V + 65599, Acc + D0 + D1 + D2).\n")},

     #{name => bitpack,
       note => "bit packing by shifts (no div/rem on the hot path): CONTROL",
       expect_reach => false,
       whole_program => false,
       src => kernel_src(?ITERS_BITPACK,
                         "-export([run/0]).\n"
                         "\n"
                         "run() -> loop(?ITERS, 0, 0).\n"
                         "\n"
                         "loop(0, _, Acc) ->\n"
                         "    Acc;\n"
                         "loop(N, I, Acc) ->\n"
                         "    B = I band 16#FF,\n"
                         "    W = (B bsl 8) bor (B bsr 2),\n"
                         "    loop(N - 1, I + 1, Acc bxor W).\n")}].

%% A kernel module: `-module(~s)' is filled in per variant, and ?ITERS is
%% defined here so the tuning lives in one place.
-spec kernel_src(pos_integer(), string()) -> string().
kernel_src(Iters, Body) ->
    lists:flatten(
      ["-module(~s).\n",
       "-define(ITERS, ", integer_to_list(Iters), ").\n",
       Body]).

%% ===================================================================
%% Build both variants of every kernel
%% ===================================================================

-spec build(kernel(), string()) -> build().
build(#{name := Name, src := Src, note := Note,
        expect_reach := Reach, whole_program := Wp} = Kernel, BeamDir) ->
    %% `manual' is the same loop with the rewrite spelled out by hand, compiled
    %% with the flags OFF: the ceiling of what the rewrite can buy here.
    Manual = case Kernel of
                 #{manual := MSrc} -> [{manual, MSrc, off}];
                 _ -> []
             end,
    Specs = [{off, Src, off}, {on, Src, on} | Manual],
    Built = [{Variant, compile_variant(Name, mod_name(Name, Variant), VSrc,
                                      VOpts, BeamDir)}
             || {Variant, VSrc, VOpts} <- Specs],
    #{name => Name, note => Note, expect_reach => Reach, whole_program => Wp,
      mods => maps:from_list([{V, Mod} || {V, {Mod, _, _}} <- Built]),
      code => maps:from_list([{V, Code} || {V, {_, Code, _}} <- Built]),
      ops => maps:from_list([{V, ops(Bin, BeamDir)}
                             || {V, {_, _, Bin}} <- Built]),
      results => maps:from_list([{V, run_once(Mod)}
                                 || {V, {Mod, _, _}} <- Built])}.

-spec mod_name(atom(), variant()) -> atom().
mod_name(Name, Variant) ->
    list_to_atom(atom_to_list(Name) ++ "_" ++ atom_to_list(Variant)).

%% {module name, generated code chunk, beam binary}
-spec compile_variant(atom(), atom(), string(), variant(), string()) ->
          {atom(), binary(), binary()}.
compile_variant(Name, Mod, Src, OptsVariant, BeamDir) ->
    File = filename:join(BeamDir, atom_to_list(Mod) ++ ".erl"),
    ok = file:write_file(File, io_lib:format(Src, [Mod])),
    Opts = [binary, return, {i, BeamDir}] ++ variant_opts(OptsVariant),
    case compile:file(File, Opts) of
        {ok, Mod, Bin} ->
            load(Mod, BeamDir, Bin),
            {Mod, code_chunk(Bin, Name), Bin};
        {ok, Mod, Bin, []} ->
            load(Mod, BeamDir, Bin),
            {Mod, code_chunk(Bin, Name), Bin};
        {ok, Mod, Bin, Warnings} ->
            load(Mod, BeamDir, Bin),
            io:format("note: ~s compiled with ~w warning(s): ~P~n",
                      [Mod, length(Warnings), Warnings, 12]),
            {Mod, code_chunk(Bin, Name), Bin};
        Other ->
            die("cannot compile kernel " ++ atom_to_list(Name)
                ++ " (" ++ atom_to_list(OptsVariant) ++ "): "
                ++ lists:flatten(io_lib:format("~P", [Other, 20])))
    end.

%% `manual' is compiled with the OFF option set: its point is the source shape,
%% not the flags.
-spec variant_opts(variant()) -> [compile:option()].
variant_opts(off) -> [];
variant_opts(on) -> [loop_opt, loop_arith_licm];
variant_opts(manual) -> [].

%% The kernel result, used as the behavioural-equivalence check across variants:
%% the pass is allowed to make the loop faster, never to change what it computes.
-spec run_once(atom()) -> term().
run_once(Mod) ->
    Mod:run().

-spec load(atom(), string(), binary()) -> ok.
load(Mod, BeamDir, Bin) ->
    File = filename:join(BeamDir, atom_to_list(Mod) ++ ".beam"),
    ok = file:write_file(File, Bin),
    {module, Mod} = code:load_binary(Mod, File, Bin),
    ok.

%% ===================================================================
%% Reach: did the pass actually change the generated code?
%% ===================================================================

%% Hard assertion: every kernel declared to reach must have a different `Code'
%% chunk, and a control kernel must be identical.  Comparing the whole .beam
%% file would be meaningless -- `CInf' and `Dbgi' embed the option list, so the
%% files always differ between option sets.
%% Also asserts that every variant of a kernel computes the same value: the pass
%% may make the loop faster, never change what it computes, and the hand-written
%% `manual' source must be observationally identical to the source it rewrites.
-spec check_reach([build()]) -> ok.
check_reach(Built) ->
    lists:foreach(fun(#{name := Name, results := Rs}) ->
                          case lists:usort(maps:values(Rs)) of
                              [_] -> ok;
                              Values ->
                                  die("variants of " ++ atom_to_list(Name)
                                      ++ " disagree: "
                                      ++ lists:flatten(io_lib:format("~P",
                                                                     [Values, 10])))
                          end
                  end, Built),
    Bad = [Name || #{name := Name, expect_reach := true,
                     code := #{off := O, on := N}} <- Built, O =:= N],
    Unexpected = [Name || #{name := Name, expect_reach := false,
                            code := #{off := O, on := N}} <- Built, O =/= N],
    case {Bad, Unexpected} of
        {[], []} ->
            ok;
        {[_ | _], _} ->
            die("the loop pass did not change the generated code of: "
                ++ lists:join(", ", [atom_to_list(N) || N <- Bad])
                ++ " -- a timing measured on identical code proves nothing");
        {[], [_ | _]} ->
            die("control kernel(s) unexpectedly changed: "
                ++ lists:join(", ", [atom_to_list(N) || N <- Unexpected]))
    end.

-spec report_reach([build()]) -> ok.
report_reach(Built) ->
    io:format("~n reach (does the pass change the generated code?)~n"
              " ------------------------------------------------------------"
              "-------------------~n"
              " ~-16s ~-9s ~-34s~n"
              " ~-16s ~-9s ~-34s~n"
              " ------------------------------------------------------------"
              "-------------------~n",
              ["kernel", "Code", "instruction shapes OFF -> ON",
               "", "changed", ""]),
    lists:foreach(
      fun(#{name := Name, note := Note,
            code := #{off := O, on := N}, ops := #{off := Off, on := On}}) ->
              io:format(" ~-16s ~-9s ~s~n",
                        [Name, changed(O, N), shape(Off, On)]),
              io:format(" ~-16s ~s~n", ["", Note])
      end, Built),
    io:format(" ------------------------------------------------------------"
              "-------------------~n"
              " `Code' = the generated code chunk, OFF vs ON (the whole .beam file"
              " always~n differs: CInf/Dbgi embed the option list).~n", []).

-spec changed(binary(), binary()) -> string().
changed(O, N) when O =:= N -> "no";
changed(_, _) -> "YES".

%% "div/rem/* bifs removed -> bsr/band/+ introduced", counted over the whole
%% module, in the form `rem 3->0  bsr 0->3'.
-spec shape({non_neg_integer(), non_neg_integer()},
            {non_neg_integer(), non_neg_integer()}) -> iolist().
shape({ExpOff, CheapOff}, {ExpOn, CheapOn}) ->
    lists:flatten(
      io_lib:format("div/rem/* ~w -> ~w, bsl/bsr/band ~w -> ~w",
                    [ExpOff, ExpOn, CheapOff, CheapOn])).

%% Count the instructions the pass removes (integer `div'/`rem'/`*' bifs) and
%% the ones it introduces (`bsr'/`band'/`i_plus'), over every function in the
%% module.  beam_disasm:file/1 is the only entry point that accepts a binary on
%% this host, hence the on-disk round trip done by load/3.
-spec ops(binary(), string()) -> {non_neg_integer(), non_neg_integer()}.
ops(Bin, BeamDir) ->
    File = filename:join(BeamDir, "disasm_tmp.beam"),
    ok = file:write_file(File, Bin),
    {beam_file, _, _, _, _, Funs} = beam_disasm:file(File),
    ok = file:delete(File),
    Names = [instr_name(I) || {function, _, _, _, Is} <- Funs, I <- Is],
    {length([N || N <- Names, lists:member(N, ?EXPENSIVE_OPS)]),
     length([N || N <- Names, lists:member(N, ?CHEAP_OPS)])}.

instr_name({gc_bif, Op, _, _, _, _}) -> {Op, 'bif'};
instr_name({bif, Op, _, _, _}) -> {Op, 'bif'};
instr_name(I) when is_tuple(I) -> element(1, I);
instr_name(I) -> I.

%% ===================================================================
%% Timing: interleaved paired samples in ONE VM
%% ===================================================================

%% One discarded warm-up round (loads and JITs both builds, warms the allocator
%% of this process), then `Reps' measured rounds.  Within a round the OFF/ON
%% order alternates, so a monotone drift of the machine does not favour one
%% variant, and the samples of the two variants stay adjacent in time.
-spec measure(pos_integer(), [build()]) -> [timing()].
measure(Reps, Built) ->
    io:format("~n timing: 1 discarded warm-up round, then ~w measured rounds "
              "(OFF/ON order alternates)~n", [Reps]),
    _ = [sample(B) || B <- Built],
    Zero = [{Name, #{V => [] || V <- variants(B)}} || #{name := Name} = B <- Built],
    Acc = lists:foldl(fun(Rep, A) -> round(Rep, Built, A) end, Zero,
                      lists:seq(1, Reps)),
    io:format("~n"),
    Acc.

-spec round(pos_integer(), [build()], [timing()]) -> [timing()].
round(Rep, Built, Acc0) ->
    lists:foldl(
      fun(#{name := Name, mods := Mods} = B, Acc) ->
              %% Rotating the order between rounds means every variant occupies
              %% every position equally often over the run, so a warm-up or
              %% drift bias cannot favour one of them.
              Order = rotate(variants(B), Rep - 1),
              lists:foldl(
                fun(Variant, A) ->
                        Mod = maps:get(Variant, Mods),
                        {Us, _} = timer:tc(Mod, run, []),
                        add_sample(Name, Variant, Us, A)
                end, Acc, Order)
      end, Acc0, Built).

%% The timed variants of a kernel, in canonical order.
-spec variants(build()) -> [variant()].
variants(#{mods := Mods}) ->
    [V || V <- [off, on, manual], maps:is_key(V, Mods)].

%% One discarded run of every variant: loads and JITs the loop, warms the
%% allocator of this process, and takes the first GC out of the measurement.
-spec sample(build()) -> ok.
sample(#{mods := Mods}) ->
    lists:foreach(fun({_, Mod}) ->
                          _ = Mod:run(),
                          ok
                  end, lists:sort(maps:to_list(Mods))),
    ok.

-spec add_sample(atom(), variant(), non_neg_integer(), [timing()]) -> [timing()].
add_sample(Name, Variant, Us, Acc) ->
    [{K, case K of
             Name -> Samples#{Variant => maps:get(Variant, Samples) ++ [Us]};
             _ -> Samples
         end} || {K, Samples} <- Acc].

%% ===================================================================
%% Allocation
%% ===================================================================

%% The strength reductions turn expensive arithmetic into cheap arithmetic; they
%% do not change how much is allocated.  Measure it so the claim is checkable:
%% a fresh process runs the kernel once and reports its own GC statistics.
-spec measure_alloc([build()]) -> [{variant(), {integer(), integer(), integer()}}].
measure_alloc(Built) ->
    [{maps:get(V, Mods), alloc(run, maps:get(V, Mods))}
     || #{mods := Mods} <- Built, V <- [off, on]].

alloc(Fun, Mod) ->
    Parent = self(),
    Ref = make_ref(),
    spawn(fun() ->
                  {GC0, W0, _} = erlang:statistics(garbage_collection),
                  _ = Mod:Fun(),
                  {GC1, W1, _} = erlang:statistics(garbage_collection),
                  {_, Heap} = erlang:process_info(self(), total_heap_size),
                  Parent ! {Ref, {GC1 - GC0, W1 - W0, Heap}}
          end),
    receive {Ref, Res} -> Res end.

%% ===================================================================
%% Whole-program cross-check
%% ===================================================================

%% A second, deliberately different noise model: run the kernel module in a
%% fresh `erl -noshell' process per variant and read the loop time back from
%% inside that VM.  This pays VM start-up and code loading cold, so it measures
%% something other than the warm in-VM loop above; it is only used on kernels
%% whose `whole_program' flag is set.
-spec whole_program([build()], non_neg_integer()) ->
          [{atom(), [non_neg_integer()], [non_neg_integer()]}].
whole_program(_Built, 0) ->
    [];
whole_program(Built, Rounds) ->
    Selected = [B || #{whole_program := true} = B <- Built],
    case Selected of
        [] ->
            [];
        _ ->
            io:format("~n whole-program cross-check (~w alternating rounds, "
                      "fresh VM per variant, cold code)~n", [Rounds]),
            [begin
                 Off = [wp_sample(Mods, off) || _ <- lists:seq(1, Rounds)],
                 On = [wp_sample(Mods, on) || _ <- lists:seq(1, Rounds)],
                 io:format(" ~-16s done~n", [Name]),
                 {Name, Off, On}
             end
             || #{name := Name, mods := Mods} <- Selected]
    end.

wp_sample(Mods, Variant) ->
    Mod = maps:get(Variant, Mods),
    Eval = lists:flatten(
             ["T0 = erlang:monotonic_time(microsecond),"
              " _ = ", atom_to_list(Mod), ":run(),"
              " T1 = erlang:monotonic_time(microsecond),"
              " io:format(\"~w~n\", [T1 - T0]),"
              " halt(0, [{flush,true}])."]),
    Cmd = io_lib:format("~s -noshell -pa ~s -eval '~s' 2>/dev/null",
                        [os:getenv("HOST_ERL", "erl"),
                         scratch_dir(),
                         Eval]),
    Full = lists:flatten(Cmd),
    case string:trim(os:cmd(Full)) of
        "" ->
            die("whole-program run of " ++ atom_to_list(Mod)
                ++ " produced no output; command was: " ++ Full);
        Out ->
            try list_to_integer(lists:last(string:split(Out, "\n", all))) of
                N when is_integer(N), N > 0 -> N;
                Other -> die("unparseable whole-program output from "
                             ++ atom_to_list(Mod) ++ ": " ++ Out ++ " ("
                             ++ integer_to_list(Other) ++ ")")
            catch
                error:badarg ->
                    die("unparseable whole-program output from "
                        ++ atom_to_list(Mod) ++ ": " ++ Out)
            end
    end.

-spec report_whole_program([{atom(), [integer()], [integer()]}]) -> ok.
report_whole_program([]) ->
    io:format("~n whole-program cross-check: NOT RUN (see --help/CHANGELOG "
              "note: 0 rounds requested)~n", []);
report_whole_program(Rows) ->
    io:format("~n ~-16s ~10s ~10s ~8s ~10s~n"
              " ~-16s ~10s ~10s ~8s ~10s~n",
              ["kernel", "OFF us", "ON us", "ratio", "spread",
               "", "(median)", "(median)", "", ""]),
    lists:foreach(
      fun({Name, Off, On}) ->
              MedOff = median(Off),
              MedOn = median(On),
              Spread = spread(Off) + spread(On),
              io:format(" ~-16s ~10w ~10w ~8.3f ~9.1f%~n",
                        [Name, MedOff, MedOn, MedOff / MedOn, 100 * Spread])
      end, Rows),
    io:format(" cold-code runs in a fresh VM: dominated by VM start-up and code"
              " loading,~n so the ratio is only indicative; the in-VM table"
              " above is the measurement.~n", []).

%% ===================================================================
%% Reporting
%% ===================================================================

-spec report([build()], [timing()],
             [{variant(), {integer(), integer(), integer()}}]) -> ok.
report(Built, Timings, Alloc) ->
    io:format("~n RUNTIME: OFF vs ON, interleaved paired samples (~w per "
              "variant), median~n", [reps(Timings)]),
    io:format(" ------------------------------------------------------------"
              "-----------------------------------------------------~n"
              " ~-16s ~8s ~8s ~7s ~7s ~8s ~8s ~5s  ~-20s~n"
              " ------------------------------------------------------------"
              "-----------------------------------------------------~n",
              ["kernel", "OFF ms", "ON ms", "ratio", "spread", "MAN ms",
               "man/off", "Code", "gc/alloc OFF -> ON"]),
    lists:foreach(
      fun(#{name := Name} = B) ->
              #{off := Off, on := On} = Samples = find_timing(Name, Timings),
              MedOff = median(Off),
              MedOn = median(On),
              Spread = spread(Off) + spread(On),
              io:format(" ~-16s ~8.3f ~8.3f ~7.3f ~6.1f% ~8s ~8s ~5s  ~s~n",
                        [Name, MedOff / 1000, MedOn / 1000, MedOff / MedOn,
                         100 * Spread, manual_ms(Samples),
                         manual_ratio(Samples), changed_of(B),
                         alloc_of_kernel(B, Alloc)])
      end, Built),
    io:format(" ------------------------------------------------------------"
              "-----------------------------------------------------~n"
              " ratio    = OFF median / ON median, i.e. > 1 means ON is faster~n"
              " spread   = per-variant interquartile range over its median, summed"
              " for the two~n            variants; a ratio inside 1+spread is not"
              " distinguishable from noise~n"
              " MAN ms   = the same loop with the rewrite written BY HAND"
              " (`band'/`bsr'/`+'),~n            compiled with the flags OFF, -"
              " when the kernel has no hand form~n"
              " man/off  = OFF median / MANUAL median: the CEILING of what the"
              " rewrite can buy on~n            this machine and runtime -- if"
              " hand-written shifts are no faster than~n            div/rem, then"
              " no compiler-side rewrite of them can be either~n"
              " Code     = did the generated `Code' chunk change at all (the hard"
              " reach assertion)~n"
              " gc/alloc = GCs/words_reclaimed/final_heap_words of one isolated"
              " run, per variant~n"
              "            (equal counts are the expected result: the strength"
              " reductions change~n            CPU work, not allocation)~n", []).

%% The manual column only exists for kernels with a hand-written form.
manual_ms(#{manual := Manual}) ->
    lists:flatten(io_lib:format("~.3f", [median(Manual) / 1000]));
manual_ms(_) ->
    "-".

manual_ratio(#{off := Off, manual := Manual}) ->
    lists:flatten(io_lib:format("~.3f", [median(Off) / median(Manual)]));
manual_ratio(_) ->
    "-".

changed_of(#{code := #{off := O, on := N}}) ->
    changed(O, N).

alloc_of_kernel(#{mods := Mods}, Alloc) ->
    {GCO, WO, HO} = alloc_of(maps:get(off, Mods), Alloc),
    {GCN, WN, HN} = alloc_of(maps:get(on, Mods), Alloc),
    alloc_str({GCO, WO, HO}, {GCN, WN, HN}).

%% Rotate the list by N positions: used to give every variant every position in
%% the measurement order over the run.
-spec rotate([T], non_neg_integer()) -> [T].
rotate(List, N) ->
    Len = length(List),
    {A, B} = lists:split(N rem Len, List),
    B ++ A.

alloc_of(Mod, Alloc) ->
    {Mod, Res} = lists:keyfind(Mod, 1, Alloc),
    Res.

alloc_str({GCO, WO, HO}, {GCN, WN, HN}) ->
    lists:flatten(io_lib:format("~w/~w/~wK -> ~w/~w/~wK",
                                [GCO, WO, HO, GCN, WN, HN])).

reps([{_, #{off := Off}} | _]) -> length(Off);
reps([]) -> 0.

find_timing(Name, Timings) ->
    case lists:keyfind(Name, 1, Timings) of
        {Name, Samples} -> Samples;
        false -> die("no timing for " ++ atom_to_list(Name))
    end.

-spec print_header(pos_integer(), non_neg_integer()) -> ok.
print_header(Reps, Rounds) ->
    io:format(
      "~n======================================================================~n"
      " Stage-2 LOOP-OPTIMISATION RUNTIME benchmark (loop_opt + loop_arith_licm)~n"
      "======================================================================~n"
      " host OTP        : ~s (erts ~s), wordsize ~w~n"
      " compiler module : ~s~n"
      " kernels         : ~w real tail-recursive loops, compiled twice "
      "(OFF / ON)~n"
      " repetitions     : ~w measured + 1 discarded warm-up round, "
      "interleaved~n"
      " whole-program   : ~w rounds (fresh VM per variant)~n"
      " host load       : ~s"
      "======================================================================~n"
      " CAVEAT: this host is shared and heavily loaded.  Absolute times are NOT~n"
      " reproducible; only the ON/OFF ratio measured inside this one session is a~n"
      " signal, and only together with the spread and the reach column.~n"
      "======================================================================~n",
      [erlang:system_info(otp_release), erlang:system_info(version),
       erlang:system_info(wordsize), code:which(compile), length(kernels()),
       Reps, Rounds, loadavg()]).

-spec loadavg() -> string().
loadavg() ->
    case file:read_file("/proc/loadavg") of
        {ok, Bin} -> binary_to_list(Bin);
        _ -> "(no /proc/loadavg on this host)\n"
    end.

%% ===================================================================
%% Small helpers
%% ===================================================================

%% Lower median; the repetition count is forced odd, so this is an observed
%% sample rather than an average of two.
-spec median([number()]) -> number().
median(Times) ->
    Sorted = lists:sort(Times),
    case length(Sorted) rem 2 of
        1 -> lists:nth((length(Sorted) + 1) div 2, Sorted);
        0 -> (lists:nth(length(Sorted) div 2, Sorted)
              + lists:nth(length(Sorted) div 2 + 1, Sorted)) / 2
    end.

%% Interquartile range over the median: a scale-free measure of the run's own
%% noise, used to decide whether a ratio is distinguishable from it.
-spec spread([number()]) -> float().
spread(Times) ->
    case median(Times) of
        M when M > 0 ->
            Sorted = lists:sort(Times),
            Q1 = lists:nth(max(1, round(length(Sorted) / 4)), Sorted),
            Q3 = lists:nth(max(1, round(3 * length(Sorted) / 4)), Sorted),
            (Q3 - Q1) / (2 * M);
        _ ->
            0.0
    end.

%% Hand-extract a chunk from the FOR1 container: this host's
%% beam_lib:chunks/2 cannot read this tree's beams at all, and the point here is
%% the Code chunk only anyway.  EA IFF 85 layout: "FOR1" | 32-bit big-endian
%% total size | form type ("BEAM", *not* a sized chunk) | chunks of 4-byte id |
%% 32-bit big-endian size | payload | 0..3 pad.
-spec code_chunk(binary(), atom()) -> binary().
code_chunk(Bin, Name) ->
    case lists:keyfind(?CODE_CHUNK, 1, beam_chunks(Bin)) of
        {?CODE_CHUNK, Payload} -> Payload;
        false -> die("no `Code' chunk in the BEAM file for "
                     ++ atom_to_list(Name))
    end.

beam_chunks(<<"FOR1", _Total:32, _Form:4/binary, Rest/binary>>) ->
    beam_chunks(Rest, []);
beam_chunks(Bin) ->
    die("not a BEAM file: " ++ lists:flatten(io_lib:format("~P", [Bin, 6]))).

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

-spec die(string()) -> no_return().
die(Msg) ->
    io:format(standard_error, "loop_opt_bench: harness error: ~s~n", [Msg]),
    erlang:halt(1, [{flush, true}]).
