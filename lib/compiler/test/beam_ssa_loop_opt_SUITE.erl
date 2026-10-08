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
-module(beam_ssa_loop_opt_SUITE).
-include_lib("common_test/include/ct.hrl").
-include("beam_ssa.hrl").
-include("beam_types.hrl").
-import_record(beam_ssa_opt, [opt_st]).
-export([all/0, suite/0]).
-export([init_per_suite/1, end_per_suite/1]).
-export([strength_reduction/1, induction_variable_canonicalisation/1,
         strength_reduction_declines/1, strength_reduction_equivalence/1,
         loop_opt_preserves_result/1, kill_switches/1,
         off_by_default/1, non_trivial_loops_unchanged/1,
         type_driven_ssa/1, type_driven_probe/1,
         type_driven_equivalence/1, type_driven_kill_switches/1,
         rem_to_band/1, arith_licm_ssa/1, arith_licm_declines/1,
         arith_licm_probe/1, arith_licm_equivalence/1,
         arith_licm_kill_switches/1, default_output_identical/1]).
suite() -> [{ct_hooks,[ts_install_cth]}, {timetrap,{minutes,5}}].
all() -> [strength_reduction, induction_variable_canonicalisation,
          strength_reduction_declines, strength_reduction_equivalence,
          loop_opt_preserves_result, kill_switches,
          off_by_default, non_trivial_loops_unchanged,
          type_driven_ssa, type_driven_probe,
          type_driven_equivalence, type_driven_kill_switches,
          rem_to_band, arith_licm_ssa, arith_licm_declines,
          arith_licm_probe, arith_licm_equivalence,
          arith_licm_kill_switches, default_output_identical].
init_per_suite(Config) -> Config.
end_per_suite(_Config) -> ok.

%%=========================================================================
%% Synthetic-SSA tests.
%%
%% The pass runs last in the repeated (fixpoint) phase, i.e. after the SSA
%% type analysis. At that point the CFG of a real tail-recursive function
%% still contains no optimizable loop (self-recursion is a tail *call*, not a
%% CFG cycle) and a `receive' loop's header carries no phi, so the loop and
%% induction-variable rewrites are driven here directly on hand-built linear
%% SSA: `beam_ssa_loop_opt:opt/1' is called with the same `#opt_st{}' the
%% pipeline uses, and the result is inspected as a `dssaopt' listing would
%% show it. The *input* block list is what the same function looks like
%% without `loop_opt' (where the pass is not run at all), so "with the pass"
%% is the output and "without the pass" is the input.
%%
%% The type-driven part of the strength reducer -- the facts taken from the
%% `arg_types' annotation -- is exercised here too, by annotating the
%% synthetic instructions, and end-to-end on a real module in the
%% `type_driven_*' tests below.
%%=========================================================================

strength_reduction(_Config) ->
    %% i = phi(0 outside, i + 1 inside): the entry value and the step are
    %% both non-negative, so i is a non-negative integer on every iteration.
    {In, Out} = run_pass(iv_loop(0, '+', 1)),
    {bif,'*'} = op(In, m),
    {bif,'div'} = op(In, d),
    %% i * 8 -> i bsl 3 and i div 4 -> i bsr 2, rewritten in place: same
    %% destination, same operands, only the operation and the literal change.
    #b_set{op={bif,'bsl'}, dst=#b_var{name=m},
           args=[#b_var{name=i}, #b_literal{val=3}]} = instr(Out, m),
    #b_set{op={bif,'bsr'}, dst=#b_var{name=d},
           args=[#b_var{name=i}, #b_literal{val=2}]} = instr(Out, d),
    %% Everything else in the loop is untouched.
    {bif,'+'} = op(Out, s),

    %% A decreasing induction variable starting at 9 is still a proven
    %% integer, but it can go negative, so only the multiplication may be
    %% reduced; `div' must stay behind because it truncates towards zero.
    {_, Out2} = run_pass(iv_loop(9, '-', 1)),
    {bif,'bsl'} = op(Out2, m),
    {bif,'div'} = op(Out2, d),

    %% `X * 2' is exact for every number -- a float doubles without rounding
    %% -- and becomes `X + X' when X is not known to be an integer; a
    %% product by a larger power of two is left alone in that case.
    {_, OutU} = run_pass(unknown_loop()),
    #b_set{op={bif,'+'}, dst=#b_var{name=m2},
           args=[#b_var{name=x}, #b_var{name=x}]} = instr(OutU, m2),
    {bif,'*'} = op(OutU, m8),
    {bif,'div'} = op(OutU, dv),
    ok.

induction_variable_canonicalisation(_Config) ->
    %% The same loop as above with the step written `1 + i' instead of
    %% `i + 1'.
    {In, Out} = run_pass(iv_loop_lhs(0)),
    %% The step recurrence is canonicalised into `i + 1' ...
    [#b_literal{val=1}, #b_var{name=i}] = args(In, up),
    [#b_var{name=i}, #b_literal{val=1}] = args(Out, up),
    %% ... and the induction variable is recognised, which is what licences
    %% the reduction of the values derived from it.
    #b_set{op={bif,'bsl'}, args=[#b_var{name=i}, #b_literal{val=3}]} =
        instr(Out, m),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=i}, #b_literal{val=2}]} =
        instr(Out, d),

    %% A value computed from an induction variable inherits its facts: with
    %% i >= 0 and step +1 the value `j = i + 1' is a non-negative integer on
    %% every iteration, so both reductions apply to it as well.
    {_, OutD} = run_pass(derived_loop()),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=j}, #b_literal{val=3}]} =
        instr(OutD, jm),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=j}, #b_literal{val=2}]} =
        instr(OutD, jd),
    ok.

strength_reduction_declines(_Config) ->
    %% Literals that are not a power of two, a zero divisor, a divisor of 1
    %% and a non-literal divisor: none of these is rewritten; in particular
    %% `div 0' keeps raising, and no reduction introduces a new exception.
    {_, Out} = run_pass(nonpow2_loop()),
    {bif,'*'} = op(Out, mul3),
    {bif,'div'} = op(Out, div3),
    {bif,'div'} = op(Out, div0),
    {bif,'div'} = op(Out, div1),
    {bif,'div'} = op(Out, divn2),

    %% A non-canonical inside edge (`up = p + i' where `p = n * 8' and n is
    %% itself loop carried) must make `i' *not* an induction variable, so
    %% its arithmetic is left alone. This is the regression test for the
    %% step analysis requiring every inside edge to be a `V +/- C'
    %% recurrence.
    {_, OutN} = run_pass(noncanon_loop()),
    {bif,'*'} = op(OutN, m),
    {bif,'div'} = op(OutN, d),
    ok.

%%=========================================================================
%% Functional equivalence: the same source compiled with and without
%% `loop_opt' must behave identically, including which expressions raise.
%%=========================================================================

strength_reduction_equivalence(Config) ->
    Src = sr_src(),
    Plain = load(Config, sr_plain, Src, []),
    Opt = load(Config, sr_opt, Src, [loop_opt]),
    Vals = [-1000001, -1000, -9, -3, -2, -1, 0, 1, 2, 3, 7, 8, 9, 255, 256,
            1000000, 123456789012345678901234567890,
            2.5, -2.5, 0.0, 1.0e300],
    lists:foreach(
      fun(F) ->
              lists:foreach(
                fun(V) ->
                        A = call(Plain, F, V),
                        B = call(Opt, F, V),
                        case A =:= B of
                            true -> ok;
                            false -> ct:fail({result_mismatch, F, V, A, B})
                        end
                end, Vals)
      end, [mul2, mul8, mul16, mul3, div2, div8, div16, div3,
            div0, div1, idiv2_nonneg, loop_mul]),
    ok.

%% Run `Mod:F(V)' and return the value or the {Class,Reason} of the
%% exception, so that results *and* exceptions can be compared.
call(Mod, F, V) ->
    try {ok, Mod:F(V)} catch C:R -> {C, R} end.

sr_src() ->
    "-module(sr_probe).\n"
    "-export([mul2/1, mul8/1, mul16/1, mul3/1,\n"
    "         div2/1, div8/1, div16/1, div3/1, div0/1, div1/1,\n"
    "         idiv2_nonneg/1, loop_mul/1]).\n"
    "mul2(X) -> X * 2.\n"
    "mul8(X) -> X * 8.\n"
    "mul16(X) -> X * 16.\n"
    "mul3(X) -> X * 3.\n"
    "div2(X) -> X div 2.\n"
    "div8(X) -> X div 8.\n"
    "div16(X) -> X div 16.\n"
    "div3(X) -> X div 3.\n"
    "div0(X) -> X div 0.\n"
    "div1(X) -> X div 1.\n"
    "idiv2_nonneg(X) when is_integer(X), X >= 0 -> X div 2.\n"
    "loop_mul(X) -> loop_mul(X, 20, 0).\n"
    "loop_mul(_X, 0, Acc) -> Acc;\n"
    "loop_mul(X, N, Acc) -> loop_mul(X, N - 1, Acc + X * 8 + X div 4).\n".

%%=========================================================================
%% The pre-existing end-to-end tests.
%%=========================================================================

loop_opt_preserves_result(Config) ->
    Mods = [{lp_count, counter_src()}, {lp_traverse, traverse_src()},
            {lp_nested, nested_src()}, {lp_ctor, invariant_ctor_src()}],
    lists:foreach(
      fun({Name, Src}) ->
              Plain = run(Config, Name, Src, []),
              Opt = run(Config, Name, Src, [loop_opt]),
              case {Plain, Opt} of
                  {R, R} -> ok;
                  _ -> ct:fail({result_mismatch, Name, Plain, Opt})
              end
      end, Mods).

kill_switches(Config) ->
    Src = nested_src(),
    Plain  = run(Config, kill_plain,  Src, []),
    Opt    = run(Config, kill_opt,    Src, [loop_opt]),
    NoLoop = run(Config, kill_noloop, Src, [loop_opt, no_ssa_opt_loop_opt]),
    NoSsa  = run(Config, kill_nossa,  Src, [loop_opt, no_ssa_opt]),
    case {Plain, Opt, NoLoop, NoSsa} of
        {R, R, R, R} -> ok;
        Other -> ct:fail({kill_switch_mismatch, Other})
    end.

off_by_default(Config) ->
    _ = run(Config, default_mod, counter_src(), []),
    Opts = compile:env_compiler_options(),
    case lists:member(loop_opt, Opts) of
        true -> ct:fail(loop_opt_enabled_by_env);
        false -> ok
    end.

non_trivial_loops_unchanged(Config) ->
    Src = raising_src(),
    R1 = (load(Config, lp_raising, Src, [loop_opt])):bad(3),
    R2 = (load(Config, lp_raising_plain, Src, [])):bad(3),
    case {R1, R2} of
        {R, R} when R =:= ok -> ok;
        Other -> ct:fail({raising_mismatch, Other})
    end.

%%=========================================================================
%% Type-driven strength reduction, on hand-built SSA.
%%
%% `x' has no defining instruction (it stands for a function argument or a
%% value extracted out of a term), so the structural analysis cannot classify
%% it; only the `arg_types' annotation of the instruction that uses it can.
%%=========================================================================

type_driven_ssa(_Config) ->
    NonNeg = #t_integer{elements={0,'+inf'}},
    AnyInt = #t_integer{},
    Negative = #t_integer{elements={'-inf',10}},
    Number = #t_number{elements={0,'+inf'}},

    %% A non-negative integer annotation licences both rules.
    {_, OutNN} = run_pass(ann_loop(NonNeg, NonNeg)),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=x}, #b_literal{val=3}]} =
        instr(OutNN, m),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=x}, #b_literal{val=2}]} =
        instr(OutNN, d),

    %% A plain integer annotation licences `*' -> `bsl' but not `div' ->
    %% `bsr', which needs non-negativity.
    {_, OutInt} = run_pass(ann_loop(AnyInt, AnyInt)),
    {bif,'bsl'} = op(OutInt, m),
    {bif,'div'} = op(OutInt, d),

    %% A range that reaches below zero proves integrality only.
    {_, OutNeg} = run_pass(ann_loop(Negative, Negative)),
    {bif,'bsl'} = op(OutNeg, m),
    {bif,'div'} = op(OutNeg, d),

    %% `number' covers floats, so it proves nothing; neither does a union
    %% with a non-integer member, nor a float.
    {_, OutNum} = run_pass(ann_loop(Number, Number)),
    {bif,'*'} = op(OutNum, m),
    {bif,'div'} = op(OutNum, d),
    {_, OutUni} = run_pass(ann_loop(#t_union{number=AnyInt}, #t_union{})),
    {bif,'*'} = op(OutUni, m),
    {bif,'div'} = op(OutUni, d),
    {_, OutFlt} = run_pass(ann_loop(#t_float{}, #t_float{})),
    {bif,'*'} = op(OutFlt, m),
    {bif,'div'} = op(OutFlt, d),

    %% Without an annotation the operand stays unknown, so nothing is
    %% rewritten: the annotation, not the loop, is what licences the rewrite.
    {_, OutNone} = run_pass(ann_loop(no_anno, no_anno)),
    {bif,'*'} = op(OutNone, m),
    {bif,'div'} = op(OutNone, d),

    %% A literal factor is never enough on its own: `x * 2' falls back to
    %% `x + x' (exact for a float too), and a bigger power of two is left
    %% alone.
    {_, OutUnknown} = run_pass(ann_mul2_loop()),
    #b_set{op={bif,'+'}, args=[#b_var{name=x}, #b_var{name=x}]} =
        instr(OutUnknown, m2),
    {bif,'*'} = op(OutUnknown, m8),

    %% The annotation of a *defining* instruction is used as well: `y = x + 1'
    %% is a non-negative integer because `x' is annotated as one, and that
    %% makes `y * 8' reducible even though the product itself carries no
    %% annotation.
    {_, OutDer} = run_pass(ann_derived_loop(NonNeg)),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=y}, #b_literal{val=3}]} =
        instr(OutDer, ym),
    {_, OutDerNo} = run_pass(ann_derived_loop(no_anno)),
    {bif,'*'} = op(OutDerNo, ym),

    %% Canonicalising `8 * x' into `x * 8' must move the annotation of the
    %% variable operand with it, otherwise the reduction loses its proof.
    {_, OutLhs} = run_pass(ann_lhs_mul_loop(NonNeg)),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=x}, #b_literal{val=3}]} =
        instr(OutLhs, m),
    {_, OutLhsNo} = run_pass(ann_lhs_mul_loop(no_anno)),
    {bif,'*'} = op(OutLhsNo, m),
    ok.

%%=========================================================================
%% The same, end to end on a real module.
%%
%% A `receive' loop is the only real CFG loop at this pipeline point, and a
%% guarded `receive' clause is exactly the shape in which the type analysis
%% proves an operand integral: this is the case the pass used to miss.
%%=========================================================================

type_driven_probe(Config) ->
    Plain = rcv_bifs(Config, lp_rcv_plain, []),
    Opt = rcv_bifs(Config, lp_rcv_opt, [loop_opt]),
    %% Without the option, `X * 8' and `X div 4' are `*' and `div'.
    [{'*', [8]}, {'div', [4]}, {'+', []}] = Plain,
    %% With it, they are reduced in place to `bsl 3' and `bsr 2'. Nothing is
    %% added or removed: only the operation and the literal changes.
    [{'bsl', [3]}, {'bsr', [2]}, {'+', []}] = Opt,
    %% The kill switches turn the reduction off again.
    NoLoop = rcv_bifs(Config, lp_rcv_noloop, [loop_opt, no_ssa_opt_loop_opt]),
    NoSsa = rcv_bifs(Config, lp_rcv_nossa, [loop_opt, no_ssa_opt]),
    [{'*', [8]}, {'div', [4]}, {'+', []}] = NoLoop,
    [{'*', [8]}, {'div', [4]}, {'+', []}] = NoSsa,
    ok.

%% Compile the objective's receive probe under `Opts' and return the
%% arithmetic BIFs of its `rcv/1' function.
rcv_bifs(Config, Name, Opts) ->
    Code = disasm(Config, Name, receive_probe_src(atom_to_list(Name)), Opts),
    fn_bifs(Code, rcv, 1).
%%=========================================================================
%% Functional equivalence of the type-driven reduction: the module is
%% compiled with and without `loop_opt' and both are run over a sweep of
%% values, comparing results *and* exceptions.
%%=========================================================================

type_driven_equivalence(Config) ->
    Plain = load(Config, eq_plain, drive_probe_src("eq_plain"), []),
    Opt = load(Config, eq_opt, drive_probe_src("eq_opt"), [loop_opt]),
    %% The rewrite really did happen in the `loop_opt' build, so this is not
    %% a comparison of a module with itself.
    OBifs = fn_bifs(disasm(Config, eq_disasm, drive_probe_src("eq_disasm"),
                           [loop_opt]), loop, 0),
    lists:foreach(fun(B) ->
                          case lists:member(B, OBifs) of
                              true -> ok;
                              false -> ct:fail({reduction_missing, B, OBifs})
                          end
                  end, [{'bsl', [3]}, {'bsr', [2]}, {'bsl', [4]}, {'bsr', [3]},
                        {'+', []}]),
    Vals = [-1000001, -1000, -9, -3, -2, -1, 0, 1, 2, 3, 7, 8, 9, 255, 256,
            1000000, 123456789012345678901234567890, 1 bsl 200,
            2.5, -2.5, 0.0, 1.0e300, foo, [a, b], <<1, 2:3>>, {}],
    Tags = [n, i, d, m],
    lists:foreach(
      fun(Tag) ->
              lists:foreach(
                fun(V) ->
                        Msg = {Tag, V},
                        A = call(Plain, Msg),
                        B = call(Opt, Msg),
                        case A =:= B of
                            true -> ok;
                            false -> ct:fail({result_mismatch, Msg, A, B})
                        end
                end, Vals)
      end, Tags),
    %% `{m, _}' takes an unguarded operand, so `X * 2' is rewritten to
    %% `X + X' there and the sweep above covers a float and two non-numbers
    %% raising `badarith'.
    ok.

%% `loop_opt', `loop_opt,no_ssa_opt_loop_opt' and `loop_opt,no_ssa_opt' must
%% agree -- both on the results and on the generated code.
type_driven_kill_switches(Config) ->
    Plain = run_sweep(load(Config, ks_plain, drive_probe_src("ks_plain"), [])),
    Opt = run_sweep(load(Config, ks_opt, drive_probe_src("ks_opt"), [loop_opt])),
    NoLoop = run_sweep(load(Config, ks_noloop, drive_probe_src("ks_noloop"),
                            [loop_opt, no_ssa_opt_loop_opt])),
    NoSsa = run_sweep(load(Config, ks_nossa, drive_probe_src("ks_nossa"),
                           [loop_opt, no_ssa_opt])),
    case {Plain, Opt, NoLoop, NoSsa} of
        {R, R, R, R} -> ok;
        Other -> ct:fail({kill_switch_mismatch, Other})
    end.

run_sweep(Mod) ->
    [{Tag, V, call(Mod, {Tag, V})}
     || Tag <- [n, i, d, m],
        V <- [-9, -1, 0, 1, 8, 2.5, foo]].

call(Mod, Msg) ->
    try {ok, Mod:one(Msg)} catch C:R -> {C, R} end.

%%=========================================================================
%% `X rem 2^n' -> `X band (2^n - 1)' for a proven non-negative dividend.
%%=========================================================================

rem_to_band(_Config) ->
    %% i is a non-negative induction variable, so `i rem 4' -> `i band 3'.
    {In, Out} = run_pass(rem_loop(0, '+')),
    {bif,'rem'} = op(In, r),
    #b_set{op={bif,'band'}, dst=#b_var{name=r},
           args=[#b_var{name=i}, #b_literal{val=3}]} = instr(Out, r),
    %% A decreasing induction variable can go negative, and `rem' truncates
    %% toward zero while `band' does not: -1 rem 4 = -1, -1 band 3 = 3.
    {_, OutDec} = run_pass(rem_loop(9, '-')),
    {bif,'rem'} = op(OutDec, r),
    %% Everything else is left alone: a divisor that is not a power of two, a
    %% divisor of 1 or 0 (which must keep raising), a non-literal divisor, a
    %% literal dividend and an unproven dividend.
    {_, OutE} = run_pass(rem_edge_loop()),
    {bif,'rem'} = op(OutE, r3),
    {bif,'rem'} = op(OutE, r1),
    {bif,'rem'} = op(OutE, r0),
    {bif,'rem'} = op(OutE, rv),
    {bif,'rem'} = op(OutE, rl),
    {bif,'rem'} = op(OutE, rx),
    ok.

%%=========================================================================
%% LICM of arithmetic (`loop_arith_licm'). A hoist is only accepted when the
%% operation is provably non-raising AT THE PREHEADER, i.e. every operand is
%% loop-invariant and its integrality (and, for `div'/`rem'/`bsl'/`bsr', the
%% second operand) is proven there. The annotation of an operand that the
%% loop itself narrows is loop-dependent and is not accepted.
%%=========================================================================

arith_licm_ssa(_Config) ->
    AnyInt = #t_integer{},
    %% y = x + 1, `x' loop-invariant and annotated an integer at that use:
    %% moved into the preheader (block 1), where it was not before.
    {In, Out} = run_arith(arith_loop('+', [var(x), lit(1)], anno0(AnyInt))),
    {bif,'+'} = op(In, y),
    [] = hoisted(In, y),
    [_] = hoisted(Out, y),
    [] = [I || I <- blk_is(Out, 2), is_dst(I, y)],
    %% Every hoistable operation, with a literal second operand.
    lists:foreach(
      fun({Op, Args}) ->
              {_, O} = run_arith(arith_loop(Op, Args, anno0(AnyInt))),
              case hoisted(O, y) of
                  [_] -> ok;
                  Other -> ct:fail({not_hoisted, Op, Args, Other})
              end
      end,
      [{'*', [var(x), lit(3)]}, {'-', [var(x), lit(1)]},
       {'band', [var(x), lit(7)]}, {'bor', [var(x), lit(8)]},
       {'bxor', [var(x), lit(1)]}, {'div', [var(x), lit(4)]},
       {'rem', [var(x), lit(4)]}, {'bsl', [var(x), lit(3)]},
       {'bsr', [var(x), lit(3)]}]),
    %% `x' defined outside the loop as `1 + 1' is a non-negative integer by
    %% the structural rules, so no annotation is needed ...
    {_, OutS} = run_arith(structural_arith_loop(false)),
    [_] = hoisted(OutS, y),
    %% ... and that fact is about the value, so a type test inside the loop
    %% does not invalidate it (unlike an annotation, see `arith_licm_declines').
    {_, OutN} = run_arith(structural_arith_loop(true)),
    [_] = hoisted(OutN, y),
    %% The base `loop_opt' pass never hoists arithmetic.
    {_, OutBase} = run_pass(arith_loop('+', [var(x), lit(1)], anno0(AnyInt))),
    [] = hoisted(OutBase, y),
    ok.

arith_licm_declines(_Config) ->
    AnyInt = #t_integer{},
    Both = #{arg_types => #{0 => AnyInt, 1 => AnyInt}},
    %% A divisor that is not a non-zero literal could be zero on the path the
    %% hoist adds, so it would have to keep raising there.
    [{'div', [var(x), var(v)], Both},
     {'rem', [var(x), var(v)], Both},
     {'div', [var(x), lit(0)], anno0(AnyInt)}],
    lists:foreach(
      fun({Op, Args, Anno}) ->
              {_, O} = run_arith(arith_loop(Op, Args, Anno)),
              case hoisted(O, y) of
                  [] -> ok;
                  Other -> ct:fail({unexpected_hoist, Op, Args, Other})
              end
      end,
      [{'div', [var(x), var(v)], Both},
       {'rem', [var(x), var(v)], Both},
       {'div', [var(x), lit(0)], anno0(AnyInt)},
       %% `bsl' raises `system_limit' for a huge count and `badarith' for a
       %% negative or non-integer one.
       {'bsl', [var(x), lit(1000)], anno0(AnyInt)},
       {'bsl', [var(x), lit(-1)], anno0(AnyInt)},
       {'bsr', [var(x), lit(-1)], anno0(AnyInt)},
       {'bsl', [var(x), var(v)], Both},
       %% An operand whose integrality is not proven at the preheader.
       {'+', [var(x), lit(1)], #{}},
       {'+', [var(x), lit(1)], anno0(#t_float{})},
       {'+', [var(x), lit(1)], anno0(#t_number{})},
       %% An operand defined inside the loop is not loop-invariant.
       {'+', [var(i), lit(1)], anno0(AnyInt)},
       %% An operand the loop narrows: the annotation comes from a test inside
       %% the loop and says nothing about the preheader.
       {'+', [var(x), lit(1)], narrowed_arith_loop()}]]),
    {_, OutNr} = run_arith(narrowed_arith_loop(anno0(AnyInt))),
    [] = hoisted(OutNr, y),
    ok.

%%=========================================================================
%% The same, end to end on a real module: a `receive' loop whose arithmetic
%% is computed from a loop-invariant argument that a guard OUTSIDE the loop
%% proved an integer.
%%=========================================================================

arith_licm_probe(Config) ->
    Src = arith_probe_src("lp_arith"),
    Plain = disasm(Config, lp_arith, Src, [loop_opt]),
    Arith = disasm(Config, lp_arith, Src, [loop_opt, loop_arith_licm]),
    %% The same arithmetic, moved: no operation and no literal is added or
    %% removed, only its position changes.
    PB = lists:sort(bifs(Plain, f, 1)),
    AB = lists:sort(bifs(Arith, f, 1)),
    case PB =:= AB of
        true -> ok;
        false -> ct:fail({bif_set_changed, PB, AB})
    end,
    case Plain =:= Arith of
        true -> ct:fail(arith_licm_did_not_fire);
        false -> ok
    end,
    %% `loop_opt' alone must not enable the arithmetic LICM: its output is
    %% identical to the same compile with the sub-flag killed.
    L1 = disasm(Config, lp_arith, Src, [loop_opt]),
    L2 = disasm(Config, lp_arith, Src, [loop_opt, no_ssa_opt_loop_arith_licm]),
    case L1 =:= L2 of
        true -> ok;
        false -> ct:fail(loop_opt_enabled_arith_licm)
    end,
    %% Arithmetic on a value the loop itself produced (a matching message) is
    %% not loop-invariant, so the pass is a no-op on that loop.
    NSrc = arith_msg_src("lp_arith_msg"),
    N1 = disasm(Config, lp_arith_msg, NSrc, [loop_opt]),
    N2 = disasm(Config, lp_arith_msg, NSrc, [loop_opt, loop_arith_licm]),
    case N1 =:= N2 of
        true -> ok;
        false -> ct:fail({unexpected_hoist, lp_arith_msg})
    end,
    ok.

%%=========================================================================
%% Functional equivalence of the arithmetic LICM: results *and* exceptions.
%%=========================================================================

arith_licm_equivalence(Config) ->
    Plain = load(Config, aeq_plain, arith_probe_src("aeq_plain"), []),
    Arith = load(Config, aeq_arith, arith_probe_src("aeq_arith"),
                 [loop_opt, loop_arith_licm]),
    %% The hoist really happened in the `loop_arith_licm' build, so this is
    %% not a comparison of a module with itself.
    DSrc = arith_probe_src("aeq_disasm"),
    case disasm(Config, aeq_disasm, DSrc, [loop_opt]) =:=
        disasm(Config, aeq_disasm, DSrc, [loop_opt, loop_arith_licm]) of
        true -> ct:fail(arith_licm_did_not_fire);
        false -> ok
    end,
    lists:foreach(
      fun({Msg, V}) ->
              A = arith_call(Plain, Msg, V),
              B = arith_call(Arith, Msg, V),
              case A =:= B of
                  true -> ok;
                  false -> ct:fail({result_mismatch, Msg, V, A, B})
              end
      end, [{M, V} || M <- [none, stop], V <- arith_values()]),
    ok.

%%=========================================================================
%% Kill switches of the new pass: its own flag (`no_ssa_opt_loop_arith_licm',
%% auto-derived from the registered pass name) and `no_ssa_opt'.
%%=========================================================================

arith_licm_kill_switches(Config) ->
    Src = arith_probe_src("aks_probe"),
    D = fun(Opts) -> disasm(Config, aks_probe, Src, Opts) end,
    Base = D([]),
    Loop = D([loop_opt]),
    Arith = D([loop_opt, loop_arith_licm]),
    case Arith =:= Loop of
        true -> ct:fail(arith_licm_did_not_fire);
        false -> ok
    end,
    %% The sub-flag's own kill switch restores the `loop_opt'-only code ...
    case D([loop_opt, loop_arith_licm, no_ssa_opt_loop_arith_licm]) =:= Loop of
        true -> ok;
        false -> ct:fail(arith_subflag_kill_switch_ineffective)
    end,
    %% ... and `no_ssa_opt' kills both passes at once.
    case D([loop_opt, loop_arith_licm, no_ssa_opt]) =:= Base of
        true -> ok;
        false -> ct:fail(no_ssa_opt_ineffective)
    end,
    %% The sub-flag on its own, and each of its two kill switches.
    case D([loop_arith_licm]) =:= Base of
        true -> ct:fail(arith_alone_ineffective);
        false -> ok
    end,
    case D([loop_arith_licm, no_ssa_opt_loop_arith_licm]) =:= Base of
        true -> ok;
        false -> ct:fail(arith_kill_switch_ineffective)
    end,
    case D([loop_arith_licm, no_ssa_opt]) =:= Base of
        true -> ok;
        false -> ct:fail(arith_no_ssa_opt_ineffective)
    end,
    %% The generated code of every sweep agrees across every option set.
    Sweeps = [arith_sweep(load(Config, N, Src, Opts))
              || {N, Opts} <- [{akp_base, []},
                               {akp_loop, [loop_opt]},
                               {akp_arith, [loop_opt, loop_arith_licm]},
                               {akp_sub, [loop_arith_licm]},
                               {akp_kill, [loop_arith_licm,
                                           no_ssa_opt_loop_arith_licm]},
                               {akp_nossa, [loop_opt, loop_arith_licm,
                                            no_ssa_opt]}]],
    case lists:usort(Sweeps) of
        [_] -> ok;
        _ -> ct:fail({kill_switch_result_mismatch, Sweeps})
    end,
    ok.

%%=========================================================================
%% Without a flag the pass does not run at all: the default output is
%% identical to the same compile with each pass explicitly killed.
%%=========================================================================

default_output_identical(Config) ->
    Src = arith_probe_src("dd_probe"),
    D = fun(Opts) -> disasm(Config, dd_probe, Src, Opts) end,
    Base = D([]),
    case {Base =:= D([no_ssa_opt_loop_opt]),
          Base =:= D([no_ssa_opt_loop_arith_licm])} of
        {true, true} -> ok;
        Other -> ct:fail({default_output_changed, Other})
    end.

%%=========================================================================
%% Synthetic SSA builders for the new rules.
%%=========================================================================

%% i = phi(Init outside, i Sign 1 inside); in the header: r = i rem 4.
rem_loop(Init, Sign) ->
    Up = case Sign of '+' -> {bif,'+'}; '-' -> {bif,'-'} end,
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(Init), 1}, {var(up), 3}]},
                        set(r, {bif,'rem'}, [var(i), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, Up, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(r)}}}].

%% Every `rem' shape that must be left alone, inside a loop with a valid
%% non-negative induction variable `i'.
rem_edge_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set(r3, {bif,'rem'}, [var(i), lit(3)]),
                        set(r1, {bif,'rem'}, [var(i), lit(1)]),
                        set(r0, {bif,'rem'}, [var(i), lit(0)]),
                        set(rv, {bif,'rem'}, [var(i), var(v)]),
                        set(rl, {bif,'rem'}, [lit(8), var(i)]),
                        set(rx, {bif,'rem'}, [var(x), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(r3)}}}].

%% y = Op(Args) in the loop header, with `Anno' the annotation of that
%% instruction; `x' is loop-invariant and has no definition in the function.
arith_loop(Op, Args, Anno) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set_anno(y, {bif,Op}, Args, Anno)],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(y)}}}].

%% x = 1 + 1 defined OUTSIDE the loop: a non-negative integer by the
%% structural rules. With `Test' the loop also narrows `x' with a test.
structural_arith_loop(Test) ->
    T = case Test of
            true -> [#b_set{op={bif,is_integer}, dst=var(t), args=[var(x)]}];
            false -> []
        end,
    [{1, #b_blk{is=[set(x, {bif,'+'}, [lit(1), lit(1)])], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]}] ++ T ++
                        [set(y, {bif,'+'}, [var(x), lit(1)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(y)}}}].

%% The loop narrows `x' with an `is_integer' test, so an `arg_types'
%% annotation of `x' is derived from inside the loop and proves nothing at
%% the preheader.
narrowed_arith_loop(Anno) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        #b_set{op={bif,is_integer}, dst=var(t),
                               args=[var(x)]},
                        set_anno(y, {bif,'+'}, [var(x), lit(1)], Anno)],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(y)}}}].

%%=========================================================================
%% Real-module probe sources for the arithmetic LICM, and helpers.
%%=========================================================================

%% The guard that proves `T' an integer is OUTSIDE the receive loop, so the
%% annotation at the multiplication is not loop-dependent and the product is
%% hoisted; the `after' clause keeps the loop from blocking.
arith_probe_src(Mod) ->
    io_lib:format(
      "-module(~s).\n-export([f/1]).\n"
      "f(T) when is_integer(T), T >= 0 ->\n"
      "    receive\n"
      "        stop -> T * 8\n"
      "    after 0 -> T + 1\n"
      "    end;\n"
      "f(T) -> T.\n", [Mod]).

%% The same loop, but the arithmetic operand comes out of the message and is
%% therefore defined inside the loop: nothing may be hoisted.
arith_msg_src(Mod) ->
    io_lib:format(
      "-module(~s).\n-export([g/1]).\n"
      "g(T) ->\n"
      "    receive\n"
      "        {n, X} when is_integer(X), X >= 0 -> X * 8;\n"
      "        stop -> T\n"
      "    after 0 -> T\n"
      "    end.\n", [Mod]).

arith_values() ->
    [-9, -1, 0, 1, 2, 7, 8, 1000000, 1 bsl 40, 2.5, foo, [a], {}].

arith_sweep(Mod) ->
    [{Msg, V, arith_call(Mod, Msg, V)}
     || Msg <- [none, stop], V <- arith_values()].

%% Return the value or the {Class,Reason} of the exception, so that results
%% *and* exceptions can be compared.
arith_call(Mod, Msg, V) ->
    flush(),
    _ = case Msg of
            none -> ok;
            stop -> self() ! stop
        end,
    R = try {ok, Mod:f(V)} catch C:R0 -> {C, R0} end,
    flush(),
    R.

flush() ->
    receive _ -> flush() after 0 -> ok end.

%% Run the `loop_arith_licm' pass directly on hand-built SSA.
run_arith(Linear) ->
    {#opt_st{ssa=Out}, _} =
        beam_ssa_loop_opt:opt_arith({#opt_st{ssa=Linear, args=[], cnt=100,
                                             anno=#{}}, #{}}),
    {Linear, Out}.

blk_is(Linear, L) ->
    {L, #b_blk{is=Is}} = lists:keyfind(L, 1, Linear),
    Is.

%% The instructions defining `Name' in the preheader (block 1 of the probes).
hoisted(Linear, Name) ->
    [I || I <- blk_is(Linear, 1), is_dst(I, Name)].

insns(Code, Name, Arity) ->
    case [Is || {function,N,A,_,Is} <- Code, N =:= Name, A =:= Arity] of
        [Is] -> Is;
        [] -> ct:fail({no_such_function, Name, Arity})
    end.

bifs(Code, Name, Arity) ->
    lists:filtermap(fun bif_op/1, insns(Code, Name, Arity)).

run(Config, Name, Src, Opts) ->
    Mod = load(Config, Name, Src, Opts),
    Mod:sample().

%% Compile `Src' with `Opts', load the resulting module and return its name.
load(Config, Name, Src, Opts) ->
    File = src_filename(Config, Name),
    {Mod, Bin} = compile_bin(Config, Name, Src, Opts),
    {module, Mod} = code:load_binary(Mod, File, Bin),
    Mod.

counter_src() ->
    "-module(lp_count).\n-export([sample/0, count/1]).\n"
    "sample() -> [count(N) || N <- lists:seq(0, 20)].\n"
    "count(N) -> count(N, 0).\n"
    "count(0, Acc) -> Acc;\n"
    "count(N, Acc) when N > 0 -> count(N - 1, Acc + N).\n".
traverse_src() ->
    "-module(lp_traverse).\n-export([sample/0, rev/1]).\n"
    "sample() -> [length(rev(lists:seq(1, N))) || N <- lists:seq(0, 50)].\n"
    "rev(L) -> rev(L, []).\n"
    "rev([H | T], Acc) -> rev(T, [H | Acc]);\n"
    "rev([], Acc) -> Acc.\n".
nested_src() ->
    "-module(lp_nested).\n-export([sample/0, outer/1]).\n"
    "sample() -> [outer(N) || N <- lists:seq(0, 12)].\n"
    "outer(N) -> outer(N, 0).\n"
    "outer(0, Acc) -> Acc;\n"
    "outer(N, Acc) when N > 0 -> outer(N - 1, Acc + inner(N, 0)).\n"
    "inner(0, Acc) -> Acc;\n"
    "inner(M, Acc) when M > 0 -> inner(M - 1, Acc + M).\n".
invariant_ctor_src() ->
    "-module(lp_ctor).\n-export([sample/0, f/2]).\n"
    "sample() -> [f(a, N) || N <- lists:seq(0, 25)].\n"
    "f(A, N) -> loop(A, N, 0).\n"
    "loop(_A, 0, Acc) -> Acc;\n"
    "loop(A, N, Acc) when N > 0 ->\n"
    "    T = {A},\n"
    "    A1 = element(1, T),\n"
    "    loop(A1, N - 1, {Acc, A1}).\n".
raising_src() ->
    "-module(lp_raising).\n-export([sample/0, bad/1]).\n"
    "sample() -> bad(3).\n"
    "bad(X) -> bad(X, 0).\n"
    "bad(0, _) -> ok;\n"
    "bad(N, Acc) when N > 0 ->\n"
    "    try bad(N - 1, Acc + N) of R -> R\n"
    "    catch error:E -> {E, ok} end.\n".

%%=========================================================================
%% Synthetic SSA builders and inspectors.
%%=========================================================================

%% Run the pass directly. Returns {Input, Output}, where the input is the
%% SSA as it is produced when `loop_opt' is not given.
run_pass(Linear) ->
    {#opt_st{ssa=Out}, _} =
        beam_ssa_loop_opt:opt({#opt_st{ssa=Linear, args=[], cnt=100, anno=#{}},
                               #{}}),
    {Linear, Out}.

instr(Linear, Name) ->
    Is = lists:append([BlkIs || {_L, #b_blk{is=BlkIs}} <- Linear]),
    case [I || I <- Is, is_dst(I, Name)] of
        [#b_set{}=I] -> I;
        [] -> missing
    end.

op(Linear, Name) ->
    case instr(Linear, Name) of
        #b_set{op=Op} -> Op;
        missing -> missing
    end.

args(Linear, Name) ->
    case instr(Linear, Name) of
        #b_set{args=As} -> As;
        missing -> missing
    end.

is_dst(#b_set{dst=#b_var{name=N}}, N) -> true;
is_dst(_, _) -> false.

var(N) -> #b_var{name=N}.
lit(V) -> #b_literal{val=V}.
set(V, Op, Args) -> #b_set{dst=var(V), op=Op, args=Args}.
bri(L) -> #b_br{bool=#b_literal{val=true}, succ=L, fail=L}.

%% i = phi(Init outside, i +/- Step inside); in the header
%%   m = i * 8,  d = i div 4,  s = m + d
iv_loop(Init, Sign, Step) ->
    Up = case Sign of '+' -> {bif,'+'}; '-' -> {bif,'-'} end,
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(Init), 1}, {var(up), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)]),
                        set(s, {bif,'+'}, [var(m), var(d)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, Up, [var(i), lit(Step)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(s)}}}].

%% The same loop with the step written `1 + i'.
iv_loop_lhs(Init) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(Init), 1}, {var(up), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [lit(1), var(i)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].

%% No definition for `x': it must be treated as unknown.
unknown_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set(m2, {bif,'*'}, [var(x), lit(2)]),
                        set(m8, {bif,'*'}, [var(x), lit(8)]),
                        set(dv, {bif,'div'}, [var(x), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m8)}}}].

%% Divisor/factor edge cases inside a loop with a valid induction variable.
nonpow2_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set(mul3, {bif,'*'}, [var(i), lit(3)]),
                        set(div3, {bif,'div'}, [var(i), lit(3)]),
                        set(div0, {bif,'div'}, [var(i), lit(0)]),
                        set(div1, {bif,'div'}, [var(i), lit(1)]),
                        set(divn2, {bif,'div'}, [lit(2), var(i)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(mul3)}}}].

%% j = i + 1, then j * 8 and j div 4.
derived_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set(j, {bif,'+'}, [var(i), lit(1)]),
                        set(jm, {bif,'*'}, [var(j), lit(8)]),
                        set(jd, {bif,'div'}, [var(j), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(jm)}}}].

%% up = p + i where p = n * 8 and n is itself loop carried: the inside edge
%% of `i' is not a `V +/- C' recurrence.
noncanon_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        #b_set{op=phi, dst=var(n),
                               args=[{lit(1), 1}, {var(np2), 3}]},
                        set(m, {bif,'*'}, [var(i), lit(8)]),
                        set(d, {bif,'div'}, [var(i), lit(4)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(p, {bif,'*'}, [var(n), lit(8)]),
                       set(up, {bif,'+'}, [var(p), var(i)]),
                       set(np2, {bif,'+'}, [var(n), lit(1)])],
                last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].

%%=========================================================================
%% Type-annotated SSA builders.
%%
%% `no_anno' means "no `arg_types' annotation at all", i.e. what an operand
%% whose type the analysis could not narrow looks like. Otherwise the value
%% is the type recorded for operand 0 of that instruction.
%%=========================================================================

%% x is a value with no defining instruction; m = x * 8 and d = x div 4.
ann_loop(MulType, DivType) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(m, {bif,'*'}, [var(x), lit(8)], anno0(MulType)),
                        set_anno(d, {bif,'div'}, [var(x), lit(4)],
                                 anno0(DivType))],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].

%% m2 = x * 2 and m8 = x * 8, with `x' untyped.
ann_mul2_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set(m2, {bif,'*'}, [var(x), lit(2)]),
                        set(m8, {bif,'*'}, [var(x), lit(8)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m8)}}}].

%% y = x + 1, and then ym = y * 8 with no annotation of its own: only the
%% annotation of the `+' that defines `y' can prove it.
ann_derived_loop(XType) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(y, {bif,'+'}, [var(x), lit(1)], anno0(XType)),
                        set(ym, {bif,'*'}, [var(y), lit(8)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(ym)}}}].

%% m = 8 * x, i.e. the literal on the left, with the annotation of the
%% variable operand at index 1.
ann_lhs_mul_loop(XType) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(m, {bif,'*'}, [lit(8), var(x)], anno1(XType))],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m)}}}].

set_anno(V, Op, Args, Anno) -> (set(V, Op, Args))#b_set{anno=Anno}.

anno0(no_anno) -> #{};
anno0(Type) -> #{arg_types => #{0 => Type}}.

anno1(no_anno) -> #{};
anno1(Type) -> #{arg_types => #{1 => Type}}.

%%=========================================================================
%% Real-module probes.
%%=========================================================================

%% The `receive' loop of the objective: a guarded clause over a value that
%% the type analysis can prove a non-negative integer.
receive_probe_src(Mod) ->
    io_lib:format(
      "-module(~s).\n-export([rcv/1]).\n"
      "rcv(T) ->\n"
      "    receive\n"
      "        {n, X} when is_integer(X), X >= 0 -> X * 8 + X div 4;\n"
      "        stop -> 0\n"
      "    after T -> -1\n"
      "    end.\n", [Mod]).

%% The same loop, with one clause per reduction rule (including the
%% unguarded `X * 2' -> `X + X' fallback), driven one message at a time so
%% that the test can sweep values through it. `flush/0' keeps the mailbox
%% clean whether or not the message matched.
drive_probe_src(Mod) ->
    io_lib:format(
      "-module(~s).\n-export([one/1]).\n"
      "one(Msg) ->\n"
      "    flush(),\n"
      "    self() ! Msg,\n"
      "    R = loop(),\n"
      "    flush(),\n"
      "    R.\n"
      "loop() ->\n"
      "    receive\n"
      "        {n, X} when is_integer(X), X >= 0 -> X * 8 + X div 4;\n"
      "        {i, X} when is_integer(X) -> X * 16;\n"
      "        {d, X} when is_integer(X), X >= 0 -> X div 8;\n"
      "        {m, X} -> X * 2;\n"
      "        stop -> 0\n"
      "    after 0 -> -1\n"
      "    end.\n"
      "flush() ->\n"
      "    receive _ -> flush()\n"
      "    after 0 -> ok\n"
      "    end.\n", [Mod]).

%%=========================================================================
%% Compiling, disassembling and running a probe under a set of options.
%%=========================================================================

%% Compile `Src' for `Name' and return the disassembled code chunks.
disasm(Config, Name, Src, Opts) ->
    {_Mod, Bin} = compile_bin(Config, Name, Src, Opts),
    Beam = beam_filename(Config, Name),
    ok = file:write_file(Beam, Bin),
    case beam_disasm:file(Beam) of
        {beam_file, _M, _Es, _As, _CI, Code} -> Code;
        Other -> ct:fail({disasm_failed, Name, Other})
    end.

%% The `{Operation, [LiteralOperands]}' pairs of every arithmetic BIF of the
%% `Name/Arity' function, in code order.
fn_bifs(Code, Name, Arity) ->
    case [Is || {function,N,A,_,Is} <- Code, N =:= Name, A =:= Arity] of
        [Is] -> lists:filtermap(fun bif_op/1, Is);
        [] -> ct:fail({no_such_function, Name, Arity})
    end.

bif_op({gc_bif,Op,_,_,Args,_}) -> {true, {Op, lits(Args)}};
bif_op({bif,Op,_,Args,_}) -> {true, {Op, lits(Args)}};
bif_op(_) -> false.

lits(Args) -> [V || {integer,V} <- Args].

%% Write `Src' next to the other test artifacts and compile it.
compile_bin(Config, Name, Src, Opts) ->
    File = source_filename(Config, Name, Src),
    Res = compile:file(File, [binary, return_errors, return_warnings | Opts]),
    case Res of
        {ok, Mod, Bin} -> {Mod, Bin};
        {ok, Mod, Bin, _Ws} -> {Mod, Bin};
        _ -> ct:fail({compile_failed, Name, Res})
    end.

source_filename(Config, Name, Src) ->
    File = src_filename(Config, Name),
    ok = file:write_file(File, Src),
    File.

src_filename(Config, Name) ->
    Priv = ?config(priv_dir, Config),
    Dir = filename:join(Priv, atom_to_list(Name)),
    ok = filelib:ensure_dir(filename:join(Dir, "x")),
    filename:join(Dir, atom_to_list(Name) ++ ".erl").

beam_filename(Config, Name) ->
    Priv = ?config(priv_dir, Config),
    Dir = filename:join(Priv, atom_to_list(Name)),
    filename:join(Dir, atom_to_list(Name) ++ ".beam").