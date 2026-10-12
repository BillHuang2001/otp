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
%% beam_ssa_opt_diff_SUITE -- a randomized differential-equivalence harness for
%% the opt-in Stage-2 SSA compiler passes (`loop_opt', `loop_arith_licm',
%% `fusion_opt', `fbip_opt').
%%
%% THE IDEA
%% The opt-in passes are default-OFF (see `beam_ssa_opt:expand_opt/2'), so a
%% build with a flag is refutable against the build without it: compile the
%% SAME probe module twice - once under the `baseline' option set and once under
%% a set that carries the flag - call every exported function with the same
%% argument battery in both builds, and require the results to agree. Every call
%% is executed in a fresh process, so a probe's own mailbox or process
%% dictionary cannot leak between calls or between builds; a normal return is
%% compared as `{ok,Value}' and an exception as `{Class,Reason}' (the stacktrace
%% is deliberately NOT part of the comparison, mirroring
%% `beam_ssa_opt_equiv_SUITE'). The comparison is exact (`=:='-style pattern
%% matching), so `0' and `0.0', or `-0.0' and `0.0', are different results.
%%
%% THE CORPUS (a randomized, template-driven generator)
%% `gen_probes/0' generates the probe corpus deterministically from the single
%% seed `?DEFAULT_SEED': the shapes of the whole corpus are drawn from one
%% deterministic stream (`shape_stream/0') and every shape randomizes its own
%% literals (multipliers, divisors, shift counts, map keys and whether a shape
%% carries a guard), so a fixed seed always yields the same corpus - and with it
%% every count this suite reports. `BEAM_SSA_OPT_DIFF_MODS' /
%% `BEAM_SSA_OPT_DIFF_FUNS' (see `env_int/2') scale it.
%%
%% Every exported probe function is ARITY 1, so one fixed battery drives all of
%% them; recursion lives in private helpers that the arity-1 wrapper calls, so
%% no probe can warn about an unused function. The probe modules always compile
%% with zero warnings (a warning fails `build/2'), and every probe terminates
%% for every battery argument: a loop is bounded by a literal or by its own
%% argument-as-list, never by an unbounded integer derived from the argument,
%% and no probe returns `self()', a ref, a port or an internally created fun -
%% those would differ between the two builds for reasons that have nothing to do
%% with the passes.
%%
%% The shape library covers, per pass:
%%   * loop strength reduction: `V * 2' (always rewritten to `V + V'), `V *
%%     (1 bsl K)' for K in 1..64 with and without an `is_integer/1'+bounded
%%     guard, `V div (1 bsl K)' and `V rem (1 bsl K)' with and without a
%%     non-negativity guard, a float multiplication, non-power-of-two
%%     multipliers, literal shifts (incl. counts that raise `system_limit' at
%%     runtime) and the reduce-candidate shapes inside funs, list
%%     comprehensions, `try' bodies and mixed arithmetic;
%%   * arithmetic LICM: a `receive' loop with loop-invariant integer arithmetic
%%     in its body, reached through the arity-1 wrapper;
%%   * Fusion 1/2 (`#{K := X} = V' extraction after an `is_map_key/2' guard),
%%     Fusion 4 (`V * K1 * K2') and Fusion 5 (`is_map_key/2' test + guard
%%     `maps:get/2'), each with its decline shapes (no `is_map/1' proof, a key
%%     mismatch), and the Fusion 6 division/remainder constant chains (a), (b)
%%     and (c) with both fire and decline constant pairs;
%%   * FBIP: a fresh record update (rewritten to the `inplace' hint), the same
%%     update on a source that escapes (sent, captured by a fun, used twice,
%%     passed in, threaded through a loop, extracted from another term), an
%%     all-fresh vs. a partially fresh `phi', a per-iteration allocation, a
%%     binary/map source (an `update_record' of a binary is folded into a
%%     `{badrecord,_}' raise) and one function that carries BOTH hints at once
%%     (`fbip_mixed'), which is what makes the FBIP rewrite visible in the
%%     `Code' chunk - a lone `inplace' hint only renames an atom of the atom
%%     table, while the presence of a second hint shifts the atom indices the
%%     code chunk encodes.
%%
%% THE OPTION MATRIX (`option_sets/0')
%% One data list of `{Label, Opts, Kind}' with three kinds of set, all starting
%% from `[binary,no_spawn_compiler_process]':
%%   * `baseline' - the reference every other set is compared against;
%%   * `transform' - a set that enables an opt-in pass; it MUST change the
%%     `Code' chunk of at least one probe module (asserted by `sensitivity/1'),
%%     which is what shows that the pass fires on the generated corpus;
%%   * `identity' - a set that must not change the generated code at all: the
%%     kill switch derived from a pass (`no_ssa_opt_fus' and friends, both alone
%%     and next to the flag they cancel) and the budget/watchdog/gate option
%%     family, which arms the driver but leaves a non-exhausted budget and a
%%     merely necessary-condition gate a no-op (asserted per probe module by
%%     `kill_switch_identity/1');
%%   * `control' - `no_ssa_opt', which disables the whole SSA optimizer: it is
%%     compared behaviourally but is not expected to be code-identical.
%%
%% THE THREE TEST CASES (group `diff')
%%   * `behaviour_equivalence/1' - the battery comparison over the whole matrix,
%%     with the compared-call and mismatch counts;
%%   * `kill_switch_identity/1' - every `identity' set must be `Code'-identical
%%     to the baseline, per probe module;
%%   * `sensitivity/1' - the per-set table of "probe modules whose `Code' chunk
%%     changed" and the assertion that every `transform' set changes at least
%%     one module.
%%
%% Everything the test cases need (the probe files, the compiled builds) is
%% produced once in `init_per_suite/1' and handed to them through the Config,
%% which both Common Test and the committed host driver
%% (`stage2_suite_driver.erl') do.
%%
%% The `Code' chunk is extracted by hand from the FOR1 container: a host
%% OTP-29 `beam_lib:chunks/2' cannot read it at all
%% (`{error,beam_lib,{unknown_chunk,...}}'). See `beam_chunks/1,2'.
%%
-module(beam_ssa_opt_diff_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([behaviour_equivalence/1,kill_switch_identity/1,sensitivity/1]).

%% The one place the corpus seed lives. It is passed to `rand:seed/2' exactly
%% once, at the start of corpus generation, so the generated corpus - and every
%% count this suite reports - is identical on every run.
-define(DEFAULT_SEED, {1013904223,1664525,22695477}).
-define(DEFAULT_SEED_ALG, exsss).

%% The corpus size, overridable through the environment (see `env_int/2').
-define(DEFAULT_MODS, 6).
-define(DEFAULT_FUNS, 12).

%% What a mismatch sample keeps: the module base name, the option-set label, the
%% function, the argument, and both results.
-define(MAX_MISMATCH_SAMPLES, 5).

suite() ->
    [{ct_hooks,[ts_install_cth]},
     {timetrap,{minutes,10}}].

all() ->
    [{group,diff}].

groups() ->
    [{diff,[],[behaviour_equivalence,kill_switch_identity,sensitivity]}].

%% Generate the corpus, write it out and compile every probe module under every
%% option set, keeping the files and the builds in the returned Config.
init_per_suite(Config) ->
    Dir = tmp_dir(Config),
    Probes = [{ModBase, write_probe(Dir, ModBase, Source)}
              || {ModBase, Source} <- gen_probes()],
    Builds = [{ModBase, Label, build(File, Opts)}
              || {ModBase, File} <- Probes,
                 {Label, Opts, _Kind} <- option_sets()],
    Config ++ [{diff, #{dir => Dir, probe_files => Probes, builds => Builds}}].

end_per_suite(_Config) ->
    ok.

init_per_group(_GroupName, Config) ->
    Config.

end_per_group(_GroupName, Config) ->
    Config.

%%============================================================================
%% Test cases
%%============================================================================

%% Call every exported arity-1 function of every probe module with every battery
%% argument, in every option set, and require every build to agree exactly with
%% the `baseline' build. Reports the corpus/option-set/call counts, the per-set
%% `Code'-chunk difference table, and fails on the first mismatches found.
behaviour_equivalence(Config) ->
    Builds = builds(Config),
    Sets = option_sets(),
    Battery = battery(),
    {Funs, Comparisons, MismatchCount, Samples} =
        compare_builds(Builds, baseline_builds(Builds), Battery),
    Cert = digests(Sets, Builds),

    ct:pal("corpus: ~w probe module(s), ~w exported arity-1 function(s); "
           "battery: ~w argument(s); option sets: ~w; builds: ~w",
           [length(probe_files(Config)), Funs, length(Battery), length(Sets),
            length(Builds)]),
    ct:pal("compared calls (one baseline execution + one build execution "
           "each): ~w (~w probe-function executions); mismatches: ~w",
           [Comparisons, Comparisons * 2, MismatchCount]),
    report_digests(Cert),

    case MismatchCount of
        0 ->
            ok;
        _ ->
            ct:fail({behaviour_mismatches, MismatchCount,
                     lists:sublist(Samples, ?MAX_MISMATCH_SAMPLES)})
    end.

%% Every `identity' set must compile every probe module to a byte-identical
%% `Code' chunk: the default-OFF path and each derived kill switch are genuine
%% no-ops on the pipeline, and a set that is not identical is a real finding.
kill_switch_identity(Config) ->
    Builds = builds(Config),
    Sets = option_sets(),
    Identity = [{Label, Opts} || {Label, Opts, identity} <- Sets],
    ?assertNotEqual([], Identity),

    Cert = digests(Sets, Builds),
    Total = length(probe_files(Config)),
    lists:foreach(
      fun({Label, Opts}) ->
              case proplists:get_value(Label, Cert) of
                  [] ->
                      ct:pal("identity ~p ~p: 0 of ~w probe module(s) changed",
                             [Label, Opts, Total]);
                  Changed ->
                      ct:fail({identity_set_changed_code, Label, Opts, Changed})
              end
      end, Identity),
    ok.

%% The per-set table of "probe modules whose `Code' chunk differs from the
%% baseline", and the assertion that every `transform' set changes at least one
%% module - i.e. that the generated corpus actually exercises the pass.
sensitivity(Config) ->
    Builds = builds(Config),
    Sets = option_sets(),
    Cert = digests(Sets, Builds),
    Total = length(probe_files(Config)),

    ct:pal("modules whose Code chunk differs from the baseline "
           "(label / opts / kind):"),
    lists:foreach(
      fun({Label, Opts, Kind}) ->
              Changed = proplists:get_value(Label, Cert),
              ct:pal("  ~-10s ~-52s ~-9s ~w of ~w~s",
                     [Label, lists:flatten(io_lib:format("~w", [Opts])), Kind,
                      length(Changed), Total, describe(Changed)])
      end, Sets ++ [{baseline, baseline_opts(), baseline}]),

    Transform = [{Label, Opts} || {Label, Opts, transform} <- Sets],
    ?assertNotEqual([], Transform),
    lists:foreach(
      fun({Label, Opts}) ->
              case proplists:get_value(Label, Cert) of
                  [_ | _] ->
                      ok;
                  [] ->
                      ct:fail({transform_set_changed_no_code, Label, Opts})
              end
      end, Transform),
    ok.

%%============================================================================
%% Config plumbing
%%============================================================================

diff(Config) ->
    case lists:keyfind(diff, 1, Config) of
        {diff, D} -> D;
        false -> ct:fail(no_diff_config)
    end.

probe_files(Config) ->
    maps:get(probe_files, diff(Config)).

builds(Config) ->
    maps:get(builds, diff(Config)).

%% The baseline build of every probe module, keyed by the module base name.
baseline_builds(Builds) ->
    maps:from_list([{ModBase, Build}
                    || {ModBase, baseline, Build} <- Builds]).

describe([]) ->
    "";
describe(Changed) ->
    lists:flatten([": ", lists:join(", ", Changed)]).

%%============================================================================
%% The option matrix
%%============================================================================

%% `{Label, Opts, Kind}'. `Opts' always start with the two options every build
%% needs: `binary' so the build is an in-memory binary, and
%% `no_spawn_compiler_process' so the compilation happens in the test process
%% (the driver is a plain `erl -noshell' node with no Common Test around it).
option_sets() ->
    B = [binary,no_spawn_compiler_process],
    [{baseline, B, baseline},

     %% Transform sets: an opt-in pass is enabled and must change the generated
     %% code of at least one probe module.
     {loop_opt, B ++ [loop_opt], transform},
     {loop_licm, B ++ [loop_arith_licm], transform},
     {loop_both, B ++ [loop_opt,loop_arith_licm], transform},
     {fusion, B ++ [fusion_opt], transform},
     {fbip, B ++ [fbip_opt], transform},
     {all_on, B ++ [loop_opt,loop_arith_licm,fusion_opt,fbip_opt], transform},

     %% Identity sets: must not change the generated code at all.
     {no_fus, B ++ [no_ssa_opt_fus], identity},
     {no_loop, B ++ [no_ssa_opt_loop_opt], identity},
     {no_licm, B ++ [no_ssa_opt_loop_arith_licm], identity},
     {no_fbip, B ++ [no_ssa_opt_fbip], identity},
     {fus_off, B ++ [fusion_opt,no_ssa_opt_fus], identity},
     {loop_off, B ++ [loop_opt,no_ssa_opt_loop_opt], identity},
     {licm_off, B ++ [loop_arith_licm,no_ssa_opt_loop_arith_licm], identity},
     {fbip_off, B ++ [fbip_opt,no_ssa_opt_fbip], identity},
     {budget, B ++ [ssa_opt_budget], identity},
     {watchdog, B ++ [ssa_opt_watchdog], identity},
     {fn_budget, B ++ [ssa_opt_fn_budget], identity},
     {gates, B ++ [ssa_opt_gates], identity},

     %% Control: the whole SSA optimizer is off. Compared behaviourally, but it
     %% is not expected to be code-identical.
     {no_ssa_opt, B ++ [no_ssa_opt], control}].

baseline_opts() ->
    case [Opts || {baseline, Opts, _} <- option_sets()] of
        [Opts] -> Opts
    end.

%%============================================================================
%% Code-chunk bookkeeping
%%============================================================================

%% For every set except the baseline: the probe modules whose `Code' chunk
%% differs from the baseline build of the same module. Returned as a proplist
%% keyed by the set label.
digests(Sets, Builds) ->
    Baseline = maps:from_list([{ModBase, code_digest(Bin)}
                               || {ModBase, baseline, {_Mod, Bin}} <- Builds]),
    [{Label,
      [ModBase || {ModBase, L, {_Mod, Bin}} <- Builds,
                  L =:= Label,
                  code_digest(Bin) =/= maps:get(ModBase, Baseline)]}
     || {Label, _Opts, _Kind} <- Sets, Label =/= baseline].

report_digests(Cert) ->
    lists:foreach(
      fun({Label, Changed}) ->
              ct:pal("option set ~p: ~w probe module(s) have a Code chunk "
                     "differing from the baseline~s",
                     [Label, length(Changed), describe(Changed)])
      end, Cert).

%%============================================================================
%% The differential comparison
%%============================================================================

%% Compare every build against the baseline build of the same probe module, over
%% every exported arity-1 function and every battery argument. Returns
%% `{ExportedFunctions, Comparisons, MismatchCount, Samples}'. `Comparisons'
%% counts each compared call once (a comparison executes the function twice -
%% once in the baseline build and once in the build under test);
%% `MismatchCount' is the exact number of disagreeing calls and `Samples' keeps
%% the first ?MAX_MISMATCH_SAMPLES of them for the failure term.
compare_builds(Builds, Baseline, Battery) ->
    {Funs, Cmp, Mis, Samples} =
        lists:foldl(
          fun({ModBase, Label, {Mod, Bin}}, {Funs0, Cmp0, Mis0, Samples0}) ->
                  {RefMod, RefBin} = maps:get(ModBase, Baseline),
                  %% Load this build once so its export table is readable;
                  %% `run/4' reloads before every call (the builds share the
                  %% module name, so only one of them can be loaded at a time).
                  load(Mod, Bin),
                  Exports = exported_funs(Mod),
                  case Label of
                      baseline ->
                          %% The baseline is the reference itself: count its
                          %% exports, compare nothing.
                          {Funs0 + length(Exports), Cmp0, Mis0, Samples0};
                      _ ->
                          {C1, M1, S1} = compare_exports(RefMod, RefBin, Mod,
                                                         Bin, ModBase, Label,
                                                         Exports, Battery,
                                                         Cmp0, Mis0, Samples0),
                          {Funs0, C1, M1, S1}
                  end
          end, {0, 0, 0, []}, Builds),
    {Funs, Cmp, Mis, lists:reverse(Samples)}.

compare_exports(RefMod, RefBin, Mod, Bin, ModBase, Label, Exports, Battery,
                Cmp0, Mis0, Samples0) ->
    lists:foldl(
      fun({Fun, _Arity}, {CmpA, MisA, SamplesA}) ->
              lists:foldl(
                fun(Arg, {CmpB, MisB, SamplesB}) ->
                        Expected = run(RefMod, RefBin, Fun, [Arg]),
                        Got = run(Mod, Bin, Fun, [Arg]),
                        CmpC = CmpB + 1,
                        case Got of
                            Expected ->
                                {CmpC, MisB, SamplesB};
                            _ ->
                                SamplesC =
                                    case length(SamplesB) < ?MAX_MISMATCH_SAMPLES of
                                        true ->
                                            [{ModBase, Label, Fun, Arg,
                                              Expected, Got} | SamplesB];
                                        false ->
                                            SamplesB
                                    end,
                                {CmpC, MisB + 1, SamplesC}
                        end
                end, {CmpA, MisA, SamplesA}, Battery)
      end, {Cmp0, Mis0, Samples0}, Exports).

%% The exported arity-1 functions of a probe module (`module_info/1' is an
%% arity-1 export too, so it is excluded by name).
exported_funs(Mod) ->
    [{F, A} || {F, A} <- Mod:module_info(exports), A =:= 1, F =/= module_info].

%% The argument battery. Deliberately mixes the shapes each probe's clauses
%% dispatch on: small and huge integers (both signs), the special floats
%% (negative zero, a denormal, the smallest subnormal, a huge value), non-number
%% atoms, a tuple, a fun, proper and improper lists, strings, maps and non-maps,
%% binaries and a non-byte-aligned bitstring. Every exported probe function is
%% arity 1, so the same battery drives all of them. Deliberately cheap: no
%% gigantic bignum - the `system_limit' path is covered by the huge LITERAL shift
%% counts inside the probes, never by an argument.
battery() ->
    [0, 1, -1, 7, 8, 3, 1 bsl 200, -(1 bsl 200),
     0.0, -0.0, 2.5, -0.5, 1.0e308, 1.0e-320, 5.0e-324,
     not_a_number, {t, 1}, fun(X) -> X end,
     [1, 2, 3], [], "abc", [1 | 2],
     #{a => 1}, #{a => 1, b => 2}, #{b => 2}, #{},
     [{a, 1}],
     <<1, 2, 3>>, <<>>, <<1:3>>].

%%============================================================================
%% The probe generator
%%============================================================================

%% The corpus, generated deterministically from ?DEFAULT_SEED: `N' probe modules
%% of `F' arity-1 shapes each, drawn from one deterministic stream so that the
%% corpus - and not just each module - is reproducible.
gen_probes() ->
    rand:seed(?DEFAULT_SEED_ALG, ?DEFAULT_SEED),
    Mods = env_int("BEAM_SSA_OPT_DIFF_MODS", ?DEFAULT_MODS),
    Funs = env_int("BEAM_SSA_OPT_DIFF_FUNS", ?DEFAULT_FUNS),
    Slots = lists:sublist(shape_stream(Mods * Funs), Mods * Funs),
    [gen_module(N, Shapes)
     || {N, Shapes} <- lists:zip(lists:seq(1, Mods), chunk(Funs, Slots))].

%% At least `N' shapes: the coverage anchors first (so that a default-sized
%% corpus contains every pass's firing shape, and one module contains both an
%% FBIP rewrite and an FBIP decline), then as many freshly shuffled rounds of
%% the whole shape library as `N' needs, so later modules mix the shapes
%% differently.
%%
%% Deliberately not an infinite lazy stream: Erlang evaluates BOTH operands of
%% `++' eagerly, so a self-referential stream definition would recurse forever
%% before a single shape could be taken from it. The number of shuffled rounds
%% is a function of `N' - i.e. of the corpus size - so the amount of randomness
%% consumed is deterministic too.
shape_stream(N) ->
    anchors() ++ shuffled_rounds(N - length(anchors())).

shuffled_rounds(Need) when Need =< 0 ->
    [];
shuffled_rounds(Need) ->
    Lib = length(all_shape_ids()),
    Rounds = (Need + Lib - 1) div Lib,
    lists:append([shuffle(all_shape_ids()) || _ <- lists:seq(1, Rounds)]).

%% A cheap deterministic shuffle: sorting on `rand:uniform/0' keys. The sort is
%% stable, so even a (astronomically unlikely) key collision keeps the result
%% reproducible.
shuffle(L) ->
    [X || {_, X} <- lists:sort([{rand:uniform(), X} || X <- L])].

%% The fixed prefix of the stream: one firing shape per opt-in pass, the FBIP
%% decline that makes the FBIP rewrite visible in the `Code' chunk (see the
%% header), and the two Fusion families.
anchors() ->
    [loop_mul2, loop_div_pow2, loop_rem_pow2,
     licm_recv, licm_recv_band,
     fusion5, fusion1, fusion4, fusion6_div,
     fbip_fresh, fbip_mixed].

chunk(_N, []) ->
    [];
chunk(N, L) ->
    Len = min(N, length(L)),
    {H, T} = lists:split(Len, L),
    [H | chunk(N, T)].

%% A probe module: one `-record/2' declaration (used by the FBIP shapes) and one
%% arity-1 function per shape, all exported.
gen_module(N, Shapes) ->
    ModBase = "beam_ssa_opt_diff_p" ++ integer_to_list(N),
    Funs = [list_to_atom("f" ++ integer_to_list(I))
            || I <- lists:seq(1, length(Shapes))],
    {ModBase,
     ["-module(", ModBase, ").\n",
      "-export([", lists:join(",", [[atom_to_list(F), "/1"] || F <- Funs]),
      "]).\n",
      "-record(r, {a, b, c}).\n\n"
      | [gen_shape(Id, F) || {Id, F} <- lists:zip(Shapes, Funs)]]}.

gen_shape(Id, Fun) ->
    case lists:keyfind(Id, 1, shapes()) of
        {Id, Gen} -> Gen(Fun);
        false -> ct:fail({unknown_shape, Id})
    end.

all_shape_ids() ->
    [Id || {Id, _Gen} <- shapes()].

%% An integer from the environment, falling back to `Default' when the variable
%% is unset, empty, not an integer or not positive.
env_int(Name, Default) ->
    case os:getenv(Name) of
        false ->
            Default;
        "" ->
            Default;
        Str ->
            try list_to_integer(Str) of
                N when N >= 1, N =< 10000 -> N;
                _ -> Default
            catch
                error:badarg -> Default
            end
    end.

%% A random element of a non-empty list.
pick(L) ->
    lists:nth(rand:uniform(length(L)), L).

%% Two distinct map keys, in either order.
two_keys() ->
    K1 = pick([a, b, c]),
    K2 = pick([K || K <- [a, b, c], K =/= K1]),
    {K1, K2}.

%% A function name as the text of the generated source.
nm(F) ->
    atom_to_list(F).

%% The name of a private helper of the shape owned by `F'.
h(F, Suffix) ->
    atom_to_list(F) ++ "_" ++ Suffix.

int(N) ->
    integer_to_list(N).

%%============================================================================
%% The shape library
%%============================================================================

%% Each shape is `{Id, fun(Fun) -> Source}', where `Fun' is the atom naming the
%% arity-1 function that carries it; the source may define private helpers named
%% after `Fun' as well. Shapes randomize their own literals.
shapes() ->
    [{loop_mul2, fun sh_loop_mul2/1},
     {loop_mul_pow2, fun sh_loop_mul_pow2/1},
     {loop_mul_pow2_noguard, fun sh_loop_mul_pow2_noguard/1},
     {loop_mul_const, fun sh_loop_mul_const/1},
     {loop_div_pow2, fun sh_loop_div_pow2/1},
     {loop_div_pow2_noguard, fun sh_loop_div_pow2_noguard/1},
     {loop_rem_pow2, fun sh_loop_rem_pow2/1},
     {loop_rem_pow2_noguard, fun sh_loop_rem_pow2_noguard/1},
     {loop_float_mul, fun sh_loop_float_mul/1},
     {loop_bsl_lit, fun sh_loop_bsl_lit/1},
     {loop_bsl_huge, fun sh_loop_bsl_huge/1},
     {loop_bsr_lit, fun sh_loop_bsr_lit/1},
     {loop_arith_mix, fun sh_loop_arith_mix/1},
     {loop_in_fun, fun sh_loop_in_fun/1},
     {loop_in_lc, fun sh_loop_in_lc/1},
     {loop_in_try, fun sh_loop_in_try/1},

     {licm_recv, fun sh_licm_recv/1},
     {licm_recv_band, fun sh_licm_recv_band/1},

     {fusion1, fun sh_fusion1/1},
     {fusion2, fun sh_fusion2/1},
     {fusion4, fun sh_fusion4/1},
     {fusion4_noguard, fun sh_fusion4_noguard/1},
     {fusion5, fun sh_fusion5/1},
     {fusion5_noproof, fun sh_fusion5_noproof/1},
     {fusion5_keydiff, fun sh_fusion5_keydiff/1},
     {fusion6_div, fun sh_fusion6_div/1},
     {fusion6_rem, fun sh_fusion6_rem/1},
     {fusion6_muldiv, fun sh_fusion6_muldiv/1},
     {fusion6_rem_decline, fun sh_fusion6_rem_decline/1},
     {fusion6_muldiv_decline, fun sh_fusion6_muldiv_decline/1},

     {fbip_fresh, fun sh_fbip_fresh/1},
     {fbip_join, fun sh_fbip_join/1},
     {fbip_phi, fun sh_fbip_phi/1},
     {fbip_loop_alloc, fun sh_fbip_loop_alloc/1},
     {fbip_mixed, fun sh_fbip_mixed/1},
     {fbip_shared_send, fun sh_fbip_shared_send/1},
     {fbip_shared_fun, fun sh_fbip_shared_fun/1},
     {fbip_shared_twice, fun sh_fbip_shared_twice/1},
     {fbip_shared_arg, fun sh_fbip_shared_arg/1},
     {fbip_shared_spin, fun sh_fbip_shared_spin/1},
     {fbip_shared_binary, fun sh_fbip_shared_binary/1},
     {fbip_shared_map, fun sh_fbip_shared_map/1},

     {misc_try_reraise, fun sh_misc_try_reraise/1},
     {misc_fun, fun sh_misc_fun/1},
     {misc_listcomp, fun sh_misc_listcomp/1},
     {misc_list_sum, fun sh_misc_list_sum/1},
     {misc_nested, fun sh_misc_nested/1}].

%%----------------------------------------------------------------------------
%% Loop strength reduction / arithmetic
%%----------------------------------------------------------------------------

%% `X * 2' -> `X + X', the one rewrite that needs no integer proof.
sh_loop_mul2(F) ->
    [nm(F), "(V) ->\n    V * 2.\n\n"].

%% `X * 2^K' -> `X bsl K': fires only for a proven, FINITE integer, so the shape
%% carries the bound in half the corpus and not in the other half.
sh_loop_mul_pow2(F) ->
    K = pick([1, 2, 3, 4, 5, 8, 16, 32, 63, 64]),
    case pick([guarded, unguarded]) of
        guarded ->
            [nm(F), "(V) when is_integer(V), V > 0, V < 1000 ->\n",
             "    V * (1 bsl ", int(K), ");\n",
             nm(F), "(V) ->\n    V.\n\n"];
        unguarded ->
            [nm(F), "(V) ->\n    V * (1 bsl ", int(K), ").\n\n"]
    end.

%% No integrality proof at all: nothing may be rewritten.
sh_loop_mul_pow2_noguard(F) ->
    K = pick([2, 3, 4, 5, 8, 16, 32, 63, 64]),
    [nm(F), "(V) ->\n    V * (1 bsl ", int(K), ").\n\n"].

%% A non-power-of-two multiplier: no strength reduction exists for it.
sh_loop_mul_const(F) ->
    K = pick([3, 5, 6, 7, 9, 10, 12, 100]),
    [nm(F), "(V) when is_integer(V) ->\n    V * ", int(K), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% `X div 2^K' -> `X bsr K', only for a proven non-negative integer.
sh_loop_div_pow2(F) ->
    K = pick([1, 2, 3, 4, 5, 6, 8, 16]),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 1000 ->\n",
     "    V div (1 bsl ", int(K), ");\n",
     nm(F), "(V) ->\n    V.\n\n"].

sh_loop_div_pow2_noguard(F) ->
    K = pick([1, 2, 3, 4, 5, 8, 16]),
    [nm(F), "(V) ->\n    V div (1 bsl ", int(K), ").\n\n"].

%% `X rem 2^K' -> `X band (2^K - 1)', only for a proven non-negative integer.
sh_loop_rem_pow2(F) ->
    K = pick([1, 2, 3, 4, 5, 6, 8, 16]),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 1000 ->\n",
     "    V rem (1 bsl ", int(K), ");\n",
     nm(F), "(V) ->\n    V.\n\n"].

sh_loop_rem_pow2_noguard(F) ->
    K = pick([1, 2, 3, 4, 5, 8, 16]),
    [nm(F), "(V) ->\n    V rem (1 bsl ", int(K), ").\n\n"].

%% A float multiplication: no integer rewrite is valid here.
sh_loop_float_mul(F) ->
    R = pick(["2.5", "-0.5", "0.1", "3.0", "-7.25"]),
    [nm(F), "(V) ->\n    V * ", R, ".\n\n"].

%% A literal shift count, including the extremes.
sh_loop_bsl_lit(F) ->
    K = pick([0, 1, 63, 64]),
    [nm(F), "(V) ->\n    V bsl ", int(K), ".\n\n"].

%% A left shift whose literal count raises `system_limit' at runtime (for a
%% non-zero operand), and a negative count, which raises `badarith'.
sh_loop_bsl_huge(F) ->
    K = pick([5000000, -5000000]),
    [nm(F), "(V) ->\n    V bsl ", int(K), ".\n\n"].

sh_loop_bsr_lit(F) ->
    K = pick([0, 1, 63, 64]),
    [nm(F), "(V) ->\n    V bsr ", int(K), ".\n\n"].

%% Mixed arithmetic on a possibly non-numeric, possibly improper operand.
sh_loop_arith_mix(F) ->
    K = pick([2, 3, 4]),
    [nm(F), "(V) ->\n    V * ", int(K), " + V.\n\n"].

%% The reduce candidate inside a fun body.
sh_loop_in_fun(F) ->
    [nm(F), "(V) ->\n    G = fun(X) -> X * ", int(pick([2, 4, 8])), " end,\n",
     "    G(V).\n\n"].

%% The reduce candidate inside a list comprehension.
sh_loop_in_lc(F) ->
    [nm(F), "(V) ->\n    [X * ", int(pick([2, 4, 8])), " || X <- V].\n\n"].

%% The reduce candidate inside a `try' body, with a re-raise in the handler.
sh_loop_in_try(F) ->
    K = pick([2, 3, 4]),
    [nm(F), "(V) ->\n    try\n        V * ", int(K), "\n    of\n        X -> X\n",
     "    catch\n        C:R:S -> erlang:raise(C, R, S)\n",
     "    after\n        ok\n    end.\n\n"].

%%----------------------------------------------------------------------------
%% Arithmetic LICM: a receive loop with loop-invariant arithmetic in its body
%%----------------------------------------------------------------------------

%% The wrapper queues exactly as many messages as the loop has iterations, so
%% the `receive' always matches immediately and never reaches its `after'
%% clause - while the `after' body, which is where the loop-invariant arithmetic
%% sits, is still generated (and still hoisted by `loop_arith_licm').
sh_licm_recv(F) ->
    L = h(F, "loop"),
    [nm(F), "(V) when is_integer(V) ->\n",
     "    self() ! {go, 0},\n    self() ! {go, 0},\n    self() ! {go, 0},\n",
     "    ", L, "(V, 3);\n",
     nm(F), "(V) ->\n    V.\n\n",
     L, "(V, N) when is_integer(V), is_integer(N), N > 0 ->\n",
     "    receive\n",
     "        {go, X} ->\n            ", L, "(V + X, N - 1);\n",
     "        stop ->\n            V\n",
     "    after 1 ->\n        V - 16#ffffffff\n    end;\n",
     L, "(V, _N) ->\n    V.\n\n"].

%% The same loop with an invariant `band'/`+' expression in the body.
sh_licm_recv_band(F) ->
    L = h(F, "loop"),
    [nm(F), "(V) when is_integer(V) ->\n",
     "    self() ! go,\n    self() ! go,\n",
     "    ", L, "(V, 2);\n",
     nm(F), "(V) ->\n    V.\n\n",
     L, "(V, N) when is_integer(V), is_integer(N), N > 0 ->\n",
     "    receive\n",
     "        go ->\n            ", L, "(V + 1, N - 1)\n",
     "    after 1 ->\n        (V band ", int(pick([127, 255, 1023])), ") + 7\n    end;\n",
     L, "(V, _N) ->\n    V.\n\n"].

%%----------------------------------------------------------------------------
%% Fusion 1 / 2: a redundant map probe before an extraction
%%----------------------------------------------------------------------------

sh_fusion1(F) ->
    K = pick([a, b, c]),
    [nm(F), "(V) when is_map(V), is_map_key(", atom_to_list(K), ", V) ->\n",
     "    #{", atom_to_list(K), " := X} = V,\n    X;\n",
     nm(F), "(_) ->\n    other.\n\n"].

sh_fusion2(F) ->
    {K1, K2} = two_keys(),
    K1s = atom_to_list(K1),
    K2s = atom_to_list(K2),
    [nm(F), "(V) when is_map(V), is_map_key(", K1s, ", V), is_map_key(", K2s,
     ", V) ->\n",
     "    #{", K1s, " := X, ", K2s, " := Y} = V,\n    {X, Y};\n",
     nm(F), "(_) ->\n    other.\n\n"].

%%----------------------------------------------------------------------------
%% Fusion 4: a multiplication constant chain
%%----------------------------------------------------------------------------

sh_fusion4(F) ->
    {K1, K2} = {pick([2, 3, 4, 5]), pick([3, 4, 6, 7])},
    [nm(F), "(V) when is_integer(V) ->\n    V * ", int(K1), " * ", int(K2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% Without the integer proof the chain may not be folded (`*' is not associative
%% for floats).
sh_fusion4_noguard(F) ->
    {K1, K2} = {pick([2, 3, 4]), pick([3, 6, 7])},
    [nm(F), "(V) ->\n    V * ", int(K1), " * ", int(K2), ".\n\n"].

%%----------------------------------------------------------------------------
%% Fusion 5: a `is_map_key/2' test plus a guard `maps:get/2'
%%----------------------------------------------------------------------------

sh_fusion5(F) ->
    K = atom_to_list(pick([a, b, c])),
    [nm(F), "(V) when is_map(V), is_map_key(", K, ", V) ->\n",
     "    maps:get(", K, ", V);\n",
     nm(F), "(_) ->\n    other.\n\n"].

%% No `is_map/1' proof, so the operand is not known to be a map: decline.
sh_fusion5_noproof(F) ->
    K = atom_to_list(pick([a, b, c])),
    [nm(F), "(V) ->\n    maps:get(", K, ", V).\n\n"].

%% The test and the lookup disagree about the key: decline.
sh_fusion5_keydiff(F) ->
    {K1, K2} = two_keys(),
    [nm(F), "(V) when is_map(V), is_map_key(", atom_to_list(K1), ", V) ->\n",
     "    maps:get(", atom_to_list(K2), ", V);\n",
     nm(F), "(_) ->\n    other.\n\n"].

%%----------------------------------------------------------------------------
%% Fusion 6: the division / remainder constant chains
%%----------------------------------------------------------------------------

%% (a) `(V div C1) div C2' -> `V div (C1 * C2)'.
sh_fusion6_div(F) ->
    C1 = pick([4, 8, 16, 32]),
    C2 = pick([2, 4, 8]),
    [nm(F), "(V) when is_integer(V) ->\n",
     "    (V div ", int(C1), ") div ", int(C2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% (b) `(V rem C1) rem C2' -> `V rem C2' when C1 is a multiple of C2.
sh_fusion6_rem(F) ->
    C2 = pick([2, 4, 8]),
    C1 = C2 * pick([2, 3, 4, 5]),
    [nm(F), "(V) when is_integer(V) ->\n",
     "    (V rem ", int(C1), ") rem ", int(C2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% (c) `(V * C1) div C2' -> `V * (C1 div C2)' when C2 divides C1; the operand has
%% to be a provably bounded integer, hence the guard.
sh_fusion6_muldiv(F) ->
    C2 = pick([2, 4, 8]),
    C1 = C2 * pick([2, 3, 4]),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 1000 ->\n",
     "    (V * ", int(C1), ") div ", int(C2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% (b) declined: C1 is not a multiple of C2 (an odd C1 against an even C2).
sh_fusion6_rem_decline(F) ->
    C1 = pick([4, 8, 16]),
    C2 = pick([3, 5, 6]),
    [nm(F), "(V) when is_integer(V) ->\n",
     "    (V rem ", int(C1), ") rem ", int(C2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%% (c) declined: C2 does not divide C1.
sh_fusion6_muldiv_decline(F) ->
    C1 = pick([3, 5, 7, 9, 15]),
    C2 = pick([2, 4, 8]),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 1000 ->\n",
     "    (V * ", int(C1), ") div ", int(C2), ";\n",
     nm(F), "(V) ->\n    V.\n\n"].

%%----------------------------------------------------------------------------
%% FBIP: fresh vs. shared `update_record' sources
%%----------------------------------------------------------------------------

%% Fire: the source is the result of a local fresh-returning call, used once and
%% not otherwise observable.
sh_fbip_fresh(F) ->
    Mk = h(F, "mk"),
    [nm(F), "(V) ->\n    R = ", Mk, "(V),\n    R#r{b = 1}.\n\n",
     Mk, "(X) ->\n    #r{a = X}.\n\n"].

%% Fire: the source is defined before an intervening branch/join and updated
%% after it (the definition block still dominates the update block).
sh_fbip_join(F) ->
    Mk = h(F, "mk"),
    [nm(F), "(V) ->\n    R = ", Mk, "(V),\n",
     "    Y = case V of\n        true -> 1;\n        _ -> 2\n    end,\n",
     "    R#r{b = Y}.\n\n",
     Mk, "(X) ->\n    #r{a = X}.\n\n"].

%% Fire: an all-fresh phi - both incoming values come from the fresh call.
sh_fbip_phi(F) ->
    Mk = h(F, "mk"),
    [nm(F), "(V) ->\n",
     "    R = case V of\n        true -> ", Mk, "(1);\n        _ -> ", Mk, "(2)\n    end,\n",
     "    R#r{b = 1}.\n\n",
     Mk, "(X) ->\n    #r{a = X}.\n\n"].

%% Fire: a fresh record is allocated on every iteration of the loop and updated
%% once per allocation. The iteration count comes from the guard (0..4), never
%% from the argument, so the loop is bounded.
sh_fbip_loop_alloc(F) ->
    Mk = h(F, "mk"),
    L = h(F, "loop"),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 5 ->\n",
     "    ", L, "(V, ", Mk, "(0));\n",
     nm(F), "(V) ->\n    V.\n\n",
     L, "(N, R) when is_integer(N), N =< 0 ->\n    R;\n",
     L, "(N, _R) when is_integer(N) ->\n",
     "    R0 = ", Mk, "(N),\n    ", L, "(N - 1, R0#r{b = N}).\n\n",
     Mk, "(X) ->\n    #r{a = X}.\n\n"].

%% Both hints in one function: the first update is in-place, the second copies
%% (its source escapes through the send). This is the shape that makes the FBIP
%% rewrite visible in the `Code' chunk.
sh_fbip_mixed(F) ->
    Mk = h(F, "mk"),
    [nm(F), "(V) ->\n    R = ", Mk, "(V),\n    R1 = R#r{b = 1},\n",
     "    {self() ! R1, R1#r{c = 2}}.\n\n",
     Mk, "(X) ->\n    #r{a = X}.\n\n"].

%% Decline: the value is handed to another process before the update.
sh_fbip_shared_send(F) ->
    [nm(F), "(V) ->\n    R = #r{a = V},\n    self() ! R,\n    R#r{b = 1}.\n\n"].

%% Decline: the value is captured by a fun, so it stays reachable.
sh_fbip_shared_fun(F) ->
    [nm(F), "(V) ->\n    R = #r{a = V},\n    G = fun() -> R end,\n",
     "    {R#r{b = 1}, G()}.\n\n"].

%% Decline: the source is used twice.
sh_fbip_shared_twice(F) ->
    [nm(F), "(V) ->\n    R = #r{a = V},\n    {R, R#r{b = 1}}.\n\n"].

%% Decline: the source is the function argument, which is never fresh.
sh_fbip_shared_arg(F) ->
    [nm(F), "(R) ->\n    R#r{a = 1}.\n\n"].

%% Decline: the value is threaded through a recursive loop. Bounded by the
%% guard, so the recursion cannot be driven by the argument.
sh_fbip_shared_spin(F) ->
    L = h(F, "spin"),
    [nm(F), "(V) when is_integer(V), V >= 0, V < 5 ->\n",
     "    ", L, "(V, #r{a = 0});\n",
     nm(F), "(V) ->\n    V.\n\n",
     L, "(0, R) ->\n    R;\n",
     L, "(N, R) when is_integer(N), N > 0 ->\n    ", L, "(N - 1, R#r{a = N});\n",
     L, "(N, _R) ->\n    N.\n\n"].

%% Decline: the source is a binary, so the update can never succeed and the type
%% analysis folds it into a `{badrecord,_}' raise.
sh_fbip_shared_binary(F) ->
    [nm(F), "(V) ->\n    B = <<V:8>>,\n    B#r{b = 1}.\n\n"].

%% Decline: the source is a map, and maps are never updated in place.
sh_fbip_shared_map(F) ->
    [nm(F), "(V) ->\n    M = #{a => V},\n    M#r{b = 1}.\n\n"].

%%----------------------------------------------------------------------------
%% Mixed shapes: guards, try/catch/after, funs, lists
%%----------------------------------------------------------------------------

%% try/catch/after nesting with a re-raise.
sh_misc_try_reraise(F) ->
    K = pick([2, 3, 4]),
    [nm(F), "(V) ->\n    try\n        V * ", int(K), "\n    of\n        X -> X\n",
     "    catch\n        C:R:S ->\n",
     "            try\n                erlang:raise(C, R, S)\n",
     "            after\n                ok\n            end\n",
     "    after\n        ok\n    end.\n\n"].

%% A closure created in the body and applied to the argument.
sh_misc_fun(F) ->
    K = pick([2, 3, 5]),
    [nm(F), "(V) ->\n    G = fun(X) -> X * ", int(K), " end,\n    G(V).\n\n"].

%% A list comprehension over the argument.
sh_misc_listcomp(F) ->
    K = pick([2, 3, 4]),
    [nm(F), "(V) ->\n    [X * ", int(K), " || X <- V].\n\n"].

%% A recursive list sum in a private helper (recursion over the argument as a
%% list, which the battery keeps short; a non-list takes the second clause).
sh_misc_list_sum(F) ->
    Add = h(F, "add"),
    [nm(F), "(V) when is_list(V) ->\n    ", Add, "(V, 0);\n",
     nm(F), "(V) ->\n    V.\n\n",
     Add, "([E | T], A) when is_integer(E) ->\n    ", Add, "(T, A + E);\n",
     Add, "([E | T], A) ->\n    ", Add, "(T, A) + (E - E);\n",
     Add, "([], A) ->\n    A.\n\n"].

%% Guards, `andalso'/`orelse', `case' and `if' on the same argument.
sh_misc_nested(F) ->
    K = pick([2, 4, 8]),
    [nm(F), "(V) when is_integer(V), V > 0, V < 1000 andalso V rem 2 =:= 0 ->\n",
     "    V * ", int(K), ";\n",
     nm(F), "(V) when is_integer(V) orelse is_float(V) ->\n",
     "    case V of\n",
     "        0 ->\n            0;\n",
     "        _ when V > 0 ->\n            V + ", int(K), ";\n",
     "        _ ->\n            V - ", int(K), "\n",
     "    end;\n",
     nm(F), "(V) when is_list(V) ->\n",
     "    if\n",
     "        V =:= [] ->\n            empty;\n",
     "        true ->\n            V\n",
     "    end;\n",
     nm(F), "(V) ->\n    V.\n\n"].

%%============================================================================
%% Running and compiling
%%============================================================================

%% Run `Mod:Fun(Args)' in the freshly loaded build. A normal return is
%% `{ok,Value}' and an exception `{Class,Reason}'; the stacktrace is not part of
%% the comparison. The call runs in a fresh process so that a probe's own
%% mailbox (the LICM receive loops) cannot leak into the test process or between
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
%% Each call gets its own process, so a `receive' that is still armed in a
%% previous process cannot observe this load.
load(Mod, Bin) ->
    _ = code:purge(Mod),
    {module, Mod} = code:load_binary(Mod, atom_to_list(Mod) ++ ".erl", Bin).

%% Compile with `return_errors,return_warnings': a probe that warns is a defect
%% of the generator (the compiler's own warnings are not part of what is being
%% tested here, and a warning would also mean the probe is not the clean shape
%% it claims to be).
build(File, Opts) ->
    case compile:file(File, [return_errors,return_warnings|Opts]) of
        {ok, Mod, Bin, []} ->
            {Mod, Bin};
        {ok, _Mod, _Bin, Warnings} ->
            ct:fail({compile_warnings, File, Opts, Warnings});
        {error, Errors, Warnings} ->
            ct:fail({compile_failed, File, Opts, Errors, Warnings})
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
