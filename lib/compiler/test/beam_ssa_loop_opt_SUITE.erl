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
         rem_to_band/1, div_rem_pow2/1, huge_mul_declines/1,
         arith_licm_ssa/1, arith_licm_declines/1, no_shift_hoist/1,
         arith_licm_probe/1, arith_licm_equivalence/1,
         arith_licm_kill_switches/1, default_output_identical/1]).
suite() -> [{ct_hooks,[ts_install_cth]}, {timetrap,{minutes,5}}].
all() -> [strength_reduction, induction_variable_canonicalisation,
          strength_reduction_declines, strength_reduction_equivalence,
          loop_opt_preserves_result, kill_switches,
          off_by_default, non_trivial_loops_unchanged,
          type_driven_ssa, type_driven_probe,
          type_driven_equivalence, type_driven_kill_switches,
          rem_to_band, div_rem_pow2, huge_mul_declines,
          arith_licm_ssa, arith_licm_declines, no_shift_hoist,
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
%%
%% Every reduction has to preserve EXCEPTIONS, not just values: `X * 2^n' is
%% only turned into `X bsl n' for a provably INTEGRAL (a float shift rounds
%% instead of raising `badarith') operand of provably BOUNDED magnitude (a
%% shift raises `system_limit' where `*' returns a bignum) and n =< 64, and
%% `div'/`rem' -> `bsr'/`band' need a non-negative dividend because `div'
%% truncates towards zero while the bit operations do not. The firing proofs
%% below therefore annotate a FINITE range, and the decline proofs cover the
%% merely non-negative, unbounded-integer, possibly-negative, float and
%% unannotated operands.
%%=========================================================================

strength_reduction(_Config) ->
    %% i = phi(0 outside, i + 1 inside): the entry value and the step are
    %% both non-negative, so i is a non-negative integer on every iteration.
    {In, Out} = run_pass(iv_loop(0, '+', 1)),
    {bif,'*'} = op(In, m),
    {bif,'div'} = op(In, d),
    %% `i div 4' -> `i bsr 2' (non-negativity is all that rule needs), but
    %% `i * 8' stays `*': a structurally proven integer is UNBOUNDED, so the
    %% shift is declined -- `bsl' raises `system_limit' past the bignum arity
    %% bound where `*' would still have returned a bignum.
    {bif,'*'} = op(Out, m),
    #b_set{op={bif,'bsr'}, dst=#b_var{name=d},
           args=[#b_var{name=i}, #b_literal{val=2}]} = instr(Out, d),
    %% Everything else in the loop is untouched.
    {bif,'+'} = op(Out, s),

    %% A FINITE, bounded operand range is what licenses the shift: it proves
    %% the result stays far below the arity bound. The rewrite is in place:
    %% same destination, same operands, only the operation and the literal
    %% change.
    Bounded = #t_integer{elements={0,1000}},
    {_, OutB} = run_pass(bounded_iv_loop(Bounded)),
    #b_set{op={bif,'bsl'}, dst=#b_var{name=m},
           args=[#b_var{name=i}, #b_literal{val=3}]} = instr(OutB, m),
    #b_set{op={bif,'bsr'}, dst=#b_var{name=d},
           args=[#b_var{name=i}, #b_literal{val=2}]} = instr(OutB, d),

    %% A decreasing induction variable starting at 9 is still a proven
    %% integer, but it can go negative and is unbounded: neither reduction
    %% applies (`div' must stay behind on both counts -- it truncates towards
    %% zero, and `bsr' floors).
    {_, Out2} = run_pass(iv_loop(9, '-', 1)),
    {bif,'*'} = op(Out2, m),
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
    %% ... and the induction variable is recognised, which is what proves it
    %% a non-negative integer and hence licences `div' -> `bsr' on the values
    %% derived from it. (`*' -> `bsl' additionally needs a bounded operand
    %% range, see `strength_reduction'.)
    #b_set{op={bif,'bsr'}, args=[#b_var{name=i}, #b_literal{val=2}]} =
        instr(Out, d),
    {bif,'*'} = op(Out, m),

    %% A value computed from an induction variable inherits its facts: with
    %% i >= 0 and step +1 the value `j = i + 1' is a non-negative integer on
    %% every iteration, so the quotient rule applies to it as well.
    {_, OutD} = run_pass(derived_loop()),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=j}, #b_literal{val=2}]} =
        instr(OutD, jd),
    {bif,'*'} = op(OutD, jm),
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
            1000000, 123456789012345678901234567890, 1 bsl 200,
            %% Floats: `X * 2' is `X + X' here, which must double exactly --
            %% signed zero, subnormals and a value whose double overflows all
            %% behave as the multiplication does.
            2.5, -2.5, 0.0, -0.0, 5.0e-324, -5.0e-324, 1.0e-320, 1.0e308,
            -1.0e308, 1.0e-300,
            %% A non-number raises `badarith' in both builds.
            foo, [a, b], <<1, 2:3>>, {}],
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
      end, [mul2, mul8, mul16, mul32, mul3, div2, div8, div16, div3,
            div0, div1, idiv2_nonneg, idiv16_nonneg, irem16_nonneg,
            loop_mul]),
    ok.

%% Run `Mod:F(V)' and return the value or the {Class,Reason} of the
%% exception, so that results *and* exceptions can be compared.
call(Mod, F, V) ->
    try {ok, Mod:F(V)} catch C:R -> {C, R} end.

sr_src() ->
    "-module(sr_probe).\n"
    "-export([mul2/1, mul8/1, mul16/1, mul32/1, mul3/1,\n"
    "         div2/1, div8/1, div16/1, div3/1, div0/1, div1/1,\n"
    "         idiv2_nonneg/1, idiv16_nonneg/1, irem16_nonneg/1,\n"
    "         loop_mul/1]).\n"
    "mul2(X) -> X * 2.\n"
    "mul8(X) -> X * 8.\n"
    "mul16(X) -> X * 16.\n"
    "mul32(X) -> X * 32.\n"
    "mul3(X) -> X * 3.\n"
    "div2(X) -> X div 2.\n"
    "div8(X) -> X div 8.\n"
    "div16(X) -> X div 16.\n"
    "div3(X) -> X div 3.\n"
    "div0(X) -> X div 0.\n"
    "div1(X) -> X div 1.\n"
    "idiv2_nonneg(X) when is_integer(X), X >= 0 -> X div 2.\n"
    "idiv16_nonneg(X) when is_integer(X), X >= 0 -> X div 16.\n"
    "irem16_nonneg(X) when is_integer(X), X >= 0 -> X rem 16.\n"
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
    Bounded = #t_integer{elements={0,1000}},
    BoundedSigned = #t_integer{elements={-1000,1000}},
    AnyInt = #t_integer{},
    Negative = #t_integer{elements={'-inf',10}},
    Number = #t_number{elements={0,'+inf'}},

    %% A FINITE bounded range licences `*' -> `bsl' (the firing proof); the
    %% non-negativity it implies licences `div' -> `bsr' as well.
    {_, OutB} = run_pass(ann_loop(Bounded, Bounded)),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=x}, #b_literal{val=3}]} =
        instr(OutB, m),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=x}, #b_literal{val=2}]} =
        instr(OutB, d),

    %% A finite range that reaches below zero licenses the shift (the
    %% magnitude is bounded) but not `bsr' (the dividend may be negative).
    {_, OutBS} = run_pass(ann_loop(BoundedSigned, BoundedSigned)),
    #b_set{op={bif,'bsl'}, args=[#b_var{name=x}, #b_literal{val=3}]} =
        instr(OutBS, m),
    {bif,'div'} = op(OutBS, d),

    %% A non-negative but UNBOUNDED annotation fires `div' -> `bsr' only: the
    %% magnitude of the shift's operand is not proven, and `bsl' would raise
    %% `system_limit' where `*' returns a bignum.
    {_, OutNN} = run_pass(ann_loop(NonNeg, NonNeg)),
    {bif,'*'} = op(OutNN, m),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=x}, #b_literal{val=2}]} =
        instr(OutNN, d),

    %% A plain (unbounded) integer annotation proves integrality only, which
    %% licenses neither rule now.
    {_, OutInt} = run_pass(ann_loop(AnyInt, AnyInt)),
    {bif,'*'} = op(OutInt, m),
    {bif,'div'} = op(OutInt, d),

    %% A range whose lower bound is not an integer proves integrality only.
    {_, OutNeg} = run_pass(ann_loop(Negative, Negative)),
    {bif,'*'} = op(OutNeg, m),
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
    %% rewritten: the annotation, not the loop, is what licenses the rewrite.
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
    %% is a non-negative integer because `x' is annotated as one, so `y div 4'
    %% is reduced even though the quotient carries no annotation of its own;
    %% `y * 8' is not, because that annotation proves the kind but no bound.
    {_, OutDer} = run_pass(ann_derived_loop(NonNeg)),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=y}, #b_literal{val=2}]} =
        instr(OutDer, yd),
    {bif,'*'} = op(OutDer, ym),
    {_, OutDerNo} = run_pass(ann_derived_loop(no_anno)),
    {bif,'*'} = op(OutDerNo, ym),
    {bif,'div'} = op(OutDerNo, yd),

    %% Canonicalising `8 * x' into `x * 8' must move the annotation of the
    %% variable operand with it, otherwise the reduction loses its proof.
    {_, OutLhs} = run_pass(ann_lhs_mul_loop(Bounded)),
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
    Src = receive_probe_src("lp_rcv"),
    %% `rcv/1' guards the operand non-negative but leaves its magnitude
    %% unbounded: `X div 4' -> `bsr 2' fires, `X * 8' stays `*'.
    Plain = rcv_bifs(Config, lp_rcv, rcv, 1, Src, []),
    [{'*', [8]}, {'div', [4]}, {'+', []}] = Plain,
    [{'*', [8]}, {'bsr', [2]}, {'+', []}] =
        rcv_bifs(Config, lp_rcv, rcv, 1, Src, [loop_opt]),
    %% `rcvb/1' additionally bounds it (`X =< 1000'), so the operand's range
    %% is finite and BOTH rules fire. Nothing is added or removed: only the
    %% operation and the literal changes.
    [{'*', [8]}, {'div', [4]}, {'+', []}] =
        rcv_bifs(Config, lp_rcv, rcvb, 1, Src, []),
    [{'bsl', [3]}, {'bsr', [2]}, {'+', []}] =
        rcv_bifs(Config, lp_rcv, rcvb, 1, Src, [loop_opt]),
    %% The kill switches turn the reductions off again.
    [{'*', [8]}, {'div', [4]}, {'+', []}] =
        rcv_bifs(Config, lp_rcv_noloop, rcv, 1, Src,
                 [loop_opt, no_ssa_opt_loop_opt]),
    [{'*', [8]}, {'div', [4]}, {'+', []}] =
        rcv_bifs(Config, lp_rcv_nossa, rcv, 1, Src, [loop_opt, no_ssa_opt]),
    ok.

%% Compile the receive probe as `Name' under `Opts' and return the arithmetic
%% BIFs of its `F'/`A' function.
rcv_bifs(Config, Name, F, A, Src, Opts) ->
    Code = disasm(Config, Name, Src, Opts),
    fn_bifs(Code, F, A).
%%=========================================================================
%% Functional equivalence of the type-driven reduction: the module is
%% compiled with and without `loop_opt' and both are run over a sweep of
%% values, comparing results *and* exceptions.
%%=========================================================================

type_driven_equivalence(Config) ->
    Plain = load(Config, eq_plain, drive_probe_src("eq_plain"), []),
    Opt = load(Config, eq_opt, drive_probe_src("eq_opt"), [loop_opt]),
    %% The rewrite really did happen in the `loop_opt' build, so this is not
    %% a comparison of a module with itself: `{n, _}' keeps its `* 8' (the
    %% operand is unbounded), `{i, _}' keeps its `* 16', `{d, _}' is `bsr 3',
    %% `{b, _}' (bounded) is `bsl 3' and `{m, _}' is `+'.
    OBifs = fn_bifs(disasm(Config, eq_disasm, drive_probe_src("eq_disasm"),
                           [loop_opt]), loop, 0),
    lists:foreach(fun(B) ->
                          case lists:member(B, OBifs) of
                              true -> ok;
                              false -> ct:fail({reduction_missing, B, OBifs})
                          end
                  end, [{'*', [8]}, {'bsl', [3]}, {'bsr', [2]}, {'*', [16]},
                        {'bsr', [3]}, {'+', []}]),
    Vals = [-1000001, -1000, -9, -3, -2, -1, 0, 1, 2, 3, 7, 8, 9, 255, 256,
            1000, 1001, 1000000, 123456789012345678901234567890, 1 bsl 200,
            2.5, -2.5, 0.0, -0.0, 5.0e-324, 1.0e308, 1.0e300,
            foo, [a, b], <<1, 2:3>>, {}],
    Tags = [n, b, i, d, m],
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
     || Tag <- [n, b, i, d, m],
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
%% `X div 2^n' -> `X bsr n' and `X rem 2^n' -> `X band (2^n - 1)' for a
%% proven non-negative integer dividend (no magnitude bound is needed: the
%% result shrinks). The gate on the shift COUNT of `*' -> `bsl' is checked
%% here too.
%%=========================================================================

div_rem_pow2(_Config) ->
    NonNeg = #t_integer{elements={0,'+inf'}},
    %% The firing proof: a non-negative integer is enough for both rules,
    %% even though its magnitude is unbounded.
    {_, Out} = run_pass(ann_divrem_loop(16, NonNeg)),
    #b_set{op={bif,'bsr'}, args=[#b_var{name=x}, #b_literal{val=4}]} =
        instr(Out, d),
    #b_set{op={bif,'band'}, args=[#b_var{name=x}, #b_literal{val=15}]} =
        instr(Out, r),
    %% An integer that may be negative proves non-negativity for neither:
    %% `-1 div 16 = -1' but `-1 bsr 4 = -1' agrees here, while `-1 rem 16 = -1'
    %% and `-1 band 15 = 15' disagree, and `-16 div 16 = -1' vs
    %% `-16 bsr 4 = -1' is fine but `-1 div 16 = 0' vs `-1 bsr 4 = -1' is not.
    %% Both are therefore declined.
    lists:foreach(
      fun(Type) ->
              {_, O} = run_pass(ann_divrem_loop(16, Type)),
              {bif,'div'} = op(O, d),
              {bif,'rem'} = op(O, r)
      end, [#t_integer{}, #t_integer{elements={'-inf',10}},
            #t_integer{elements={-1000,1000}}, #t_float{},
            #t_number{elements={0,'+inf'}}, no_anno]),

    %% The literal factor must be a power of two within the allowed shift
    %% count: 2^64 fires `bsl 64', 2^65 does not (a bigger shift could
    %% overflow where the multiplication would not).
    Bounded = #t_integer{elements={0,1000}},
    {_, Out64} = run_pass(ann_pow_loop(1 bsl 64, Bounded)),
    #b_set{op={bif,'bsl'}, dst=#b_var{name=p},
           args=[#b_var{name=x}, #b_literal{val=64}]} = instr(Out64, p),
    {_, Out65} = run_pass(ann_pow_loop(1 bsl 65, Bounded)),
    {bif,'*'} = op(Out65, p),
    %% ... and an unbounded operand declines regardless of `n'.
    {_, OutU64} = run_pass(ann_pow_loop(1 bsl 64, #t_integer{})),
    {bif,'*'} = op(OutU64, p),
    ok.

%%=========================================================================
%% A huge integer multiplied by 32 must stay a multiplication: the operand is
%% unbounded, so the `bsl' rewrite (which raises `system_limit' where `*'
%% returns a bignum) is declined, and the executed values agree with the
%% unoptimized build right up to the emulator's bignum arity bound.
%%=========================================================================

huge_mul_declines(Config) ->
    AnyInt = #t_integer{},
    NonNeg = #t_integer{elements={0,'+inf'}},
    %% SSA-level: no annotation, an unbounded integer and a merely
    %% non-negative range all decline `* 32' -> `bsl 5'; only a finite range
    %% fires it.
    lists:foreach(
      fun(Type) ->
              {_, O} = run_pass(ann_pow_loop(32, Type)),
              {bif,'*'} = op(O, p)
      end, [no_anno, AnyInt, NonNeg]),
    {_, OutB} = run_pass(ann_pow_loop(32, #t_integer{elements={0,1000}})),
    #b_set{op={bif,'bsl'}, dst=#b_var{name=p},
           args=[#b_var{name=x}, #b_literal{val=5}]} = instr(OutB, p),

    %% Executed: the probe's `mul32/1' keeps its `* 32'.
    Src = huge_mul_src(),
    Plain = load(Config, hm_plain, Src, []),
    Opt = load(Config, hm_opt, Src, [loop_opt]),
    [{'*', [32]}] =
        fn_bifs(disasm(Config, hm_disasm, Src, [loop_opt]), mul32, 1),
    %% The largest magnitude a bignum of this emulator can hold.
    Max = bsl_limit(),
    Boundary = 1 bsl (Max - 5),          %% `Boundary * 32' is exactly at the bound
    Over = 1 bsl (Max - 4),              %% `Over * 32' (and `Over bsl 5') overflow
    case try Over bsl 5 catch error:E -> {shift_error, E} end of
        {shift_error, system_limit} -> ok;
        Other0 -> ct:fail({expected_shift_overflow, Other0})
    end,
    lists:foreach(
      fun(V) ->
              A = call(Plain, mul32, V),
              B = call(Opt, mul32, V),
              case A =:= B of
                  true -> ok;
                  false -> ct:fail({result_mismatch, mul32, V, A, B})
              end
      end, [1, -1, 0, 1 bsl 200, -(1 bsl 200), Boundary, Over]),
    {ok, Prod} = call(Plain, mul32, Boundary),
    case Prod =:= Boundary * 32 of
        true -> ok;
        false -> ct:fail({inexact_product, Boundary, Prod})
    end,
    %% At the boundary the unoptimized build's `*' also raises, and the
    %% rewritten build must raise exactly the same thing.
    {error, system_limit} = call(Plain, mul32, Over),
    {error, system_limit} = call(Opt, mul32, Over),
    ok.

%%=========================================================================
%% LICM of arithmetic (`loop_arith_licm'). A hoist is only accepted when the
%% operation is provably non-raising AT THE PREHEADER, i.e. every operand is
%% loop-invariant and its integrality (and, for `div'/`rem', a non-zero
%% literal divisor) is proven there. The annotation of an operand that the
%% loop itself narrows is loop-dependent and is not accepted. `bsl'/`bsr' are
%% never hoisted at all: a shift can raise `system_limit' on a path the
%% original program never took (see `no_shift_hoist').
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
       {'rem', [var(x), lit(4)]}]),
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
       %% `bsl' raises `system_limit' past the bignum arity bound (a proof
       %% that excludes that is not available here) and `badarith' for a
       %% negative or non-integer count; `bsr' raises `badarith' for a
       %% negative count. Even the ordinary, entirely non-raising shapes are
       %% left in the loop, because the shift is excluded outright.
       {'bsl', [var(x), lit(3)], anno0(AnyInt)},
       {'bsr', [var(x), lit(3)], anno0(AnyInt)},
       {'bsl', [var(x), lit(1000)], anno0(AnyInt)},
       {'bsl', [var(x), lit(-1)], anno0(AnyInt)},
       {'bsr', [var(x), lit(-1)], anno0(AnyInt)},
       {'bsl', [var(x), var(v)], Both},
       {'bsr', [var(x), var(v)], Both},
       %% An operand whose integrality is not proven at the preheader.
       {'+', [var(x), lit(1)], #{}},
       {'+', [var(x), lit(1)], anno0(#t_float{})},
       {'+', [var(x), lit(1)], anno0(#t_number{})},
       %% An operand defined inside the loop is not loop-invariant.
       {'+', [var(i), lit(1)], anno0(AnyInt)},
       %% An operand the loop narrows: the annotation comes from a test inside
       %% the loop and says nothing about the preheader.
       {'+', [var(x), lit(1)], narrowed_arith_loop(anno0(AnyInt))}]),
    {_, OutNr} = run_arith(narrowed_arith_loop(anno0(AnyInt))),
    [] = hoisted(OutNr, y),
    ok.

%%=========================================================================
%% `bsl'/`bsr' are never hoisted (see `arith_licm_declines' for the SSA-level
%% proof): a shift that is loop-invariant can still raise `system_limit' or
%% `badarith' on a path the original program never took, so moving it into
%% the preheader would raise where the loop body was never entered. The
%% probes below put the shift in a `receive' clause body whose operand (and
%% count) come from outside the loop, and check that the `loop_arith_licm'
%% build is instruction-for-instruction equal to the `loop_opt'-only build
%% and behaves identically, including on the raising values.
%%=========================================================================

no_shift_hoist(Config) ->
    Src = shift_probe_src(),
    D = fun(Opts) -> disasm(Config, nsh_probe, Src, Opts) end,
    Base = D([loop_opt]),
    Licm = D([loop_opt, loop_arith_licm]),
    %% The shift functions are untouched by the LICM ...
    lists:foreach(
      fun({F, A}) ->
              case insns(Base, F, A) =:= insns(Licm, F, A) of
                  true -> ok;
                  false -> ct:fail({shift_was_hoisted, F, A})
              end
      end, [{shl, 1}, {shl2, 2}, {shr2, 2}]),
    %% ... while the control's multiplication IS hoisted, so the pass really
    %% did run.
    case insns(Base, mul8, 1) =:= insns(Licm, mul8, 1) of
        true -> ct:fail(arith_licm_did_not_fire);
        false -> ok
    end,
    %% Executed: results *and* exceptions agree. `Huge' is a magnitude at
    %% which `Huge bsl 3' overflows; with no `stop' in the mailbox the clause
    %% body is never evaluated, so it must not raise.
    Plain = load(Config, nsh_plain, Src, []),
    Opt = load(Config, nsh_opt, Src, [loop_opt, loop_arith_licm]),
    Max = bsl_limit(),
    Huge = 1 bsl (Max - 2),
    {error, system_limit} = shift_call(Plain, shl, [Huge], true),
    Cases = [ {shl, [Huge], false}, {shl, [Huge], true},
              {shl, [1], false}, {shl, [1], true},
              {shl2, [1, -1], false}, {shl2, [1, -1], true},
              {shl2, [Huge, -1], false}, {shl2, [1, 1 bsl 40], true},
              {shl2, [0, -1], false}, {shl2, [1, 3], false},
              {shr2, [1, -1], false}, {shr2, [1, -1], true},
              {shr2, [Huge, 3], false}, {shr2, [-1, 3], false},
              {shr2, [1, 1], true} ],
    lists:foreach(
      fun({F, Args, Send}) ->
              A = shift_call(Plain, F, Args, Send),
              B = shift_call(Opt, F, Args, Send),
              case A =:= B of
                  true -> ok;
                  false -> ct:fail({result_mismatch, F, Args, Send, A, B})
              end
      end, Cases),
    %% The huge clause is declined, so the shifted value itself is unchanged.
    {ok, Huge} = shift_call(Plain, shl, [Huge], false),
    {ok, Huge} = shift_call(Opt, shl, [Huge], false),
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
    %% ... while `no_ssa_opt' disables the whole `beam_ssa_opt' pass, not just
    %% the opt-in loop sub-passes, so both become inert together: the loop
    %% flags add nothing to a build that already has all SSA optimization off.
    case D([loop_opt, loop_arith_licm, no_ssa_opt]) =:= D([no_ssa_opt]) of
        true -> ok;
        false -> ct:fail(no_ssa_opt_ineffective)
    end,
    %% The two auto-derived per-pass kill switches together, on the other
    %% hand, do restore the default output: the loop passes are the only
    %% difference from a default build.
    case D([loop_opt, loop_arith_licm,
            no_ssa_opt_loop_opt, no_ssa_opt_loop_arith_licm]) =:= Base of
        true -> ok;
        false -> ct:fail(loop_kill_switches_ineffective)
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
    case D([loop_arith_licm, no_ssa_opt]) =:= D([no_ssa_opt]) of
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

%% `iv_loop' with `Type' (a FINITE `#t_integer{}' range) annotated on the
%% operand of the product and of the quotient: that bounded range is what
%% licenses `* 8' -> `bsl 3', in addition to the non-negativity the induction
%% variable already proves.
bounded_iv_loop(Type) ->
    A = anno0(Type),
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[#b_set{op=phi, dst=var(i),
                           args=[{lit(0), 1}, {var(up), 3}]},
                        set_anno(m, {bif,'*'}, [var(i), lit(8)], A),
                        set_anno(d, {bif,'div'}, [var(i), lit(4)], A)],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[set(up, {bif,'+'}, [var(i), lit(1)])], last=bri(2)}},
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

%% d = x div K and r = x rem K, both with the operand annotated `Type'.
ann_divrem_loop(K, Type) ->
    A = anno0(Type),
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(d, {bif,'div'}, [var(x), lit(K)], A),
                        set_anno(r, {bif,'rem'}, [var(x), lit(K)], A)],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(d)}}}].

%% p = x * K, with the operand annotated `Type'.
ann_pow_loop(K, Type) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(p, {bif,'*'}, [var(x), lit(K)], anno0(Type))],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(p)}}}].

%% m2 = x * 2 and m8 = x * 8, with `x' untyped.
ann_mul2_loop() ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set(m2, {bif,'*'}, [var(x), lit(2)]),
                        set(m8, {bif,'*'}, [var(x), lit(8)])],
                last=#b_br{bool=var(c), succ=3, fail=4}}},
     {3, #b_blk{is=[], last=bri(2)}},
     {4, #b_blk{is=[], last=#b_ret{arg=var(m8)}}}].

%% y = x + 1, then ym = y * 8 and yd = y div 4, with no annotation of their
%% own: only the annotation of the `+' that defines `y' can classify it, and
%% that proves the kind (so `yd' is reduced) but no bound (so `ym' is not).
ann_derived_loop(XType) ->
    [{1, #b_blk{is=[], last=bri(2)}},
     {2, #b_blk{is=[set_anno(y, {bif,'+'}, [var(x), lit(1)], anno0(XType)),
                        set(ym, {bif,'*'}, [var(y), lit(8)]),
                        set(yd, {bif,'div'}, [var(y), lit(4)])],
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

%% A `receive' loop with one guarded clause per reduction rule: a value the
%% type analysis can prove a non-negative integer. `rcv/1' leaves the
%% magnitude unbounded (so only `div' -> `bsr' fires), while `rcvb/1' bounds
%% it with `X =< 1000' (so the operand's range is finite and `*' -> `bsl'
%% fires too).
receive_probe_src(Mod) ->
    io_lib:format(
      "-module(~s).\n-export([rcv/1, rcvb/1]).\n"
      "rcv(T) ->\n"
      "    receive\n"
      "        {n, X} when is_integer(X), X >= 0 -> X * 8 + X div 4;\n"
      "        stop -> 0\n"
      "    after T -> -1\n"
      "    end.\n"
      "rcvb(T) ->\n"
      "    receive\n"
      "        {n, X} when is_integer(X), X >= 0, X =< 1000 -> X * 8 + X div 4;\n"
      "        stop -> 0\n"
      "    after T -> -1\n"
      "    end.\n", [Mod]).

%% The same loop, with one clause per reduction rule (including the
%% unguarded `X * 2' -> `X + X' fallback and a bounded `* 8' that IS turned
%% into a shift), driven one message at a time so that the test can sweep
%% values through it. `flush/0' keeps the mailbox clean whether or not the
%% message matched.
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
      "        {b, X} when is_integer(X), X >= 0, X =< 1000 -> X * 8;\n"
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

%% The probe of `huge_mul_declines': an unbounded `* 32' that must stay a
%% multiplication.
huge_mul_src() ->
    "-module(hm_probe).\n-export([mul32/1]).\n"
    "mul32(X) -> X * 32.\n".

%% The probe of `no_shift_hoist': the shift operand (and, in the `/2'
%% functions, the count) is a function argument proven an integer OUTSIDE the
%% loop, so the shift is loop-invariant and was hoisted before the fix; the
%% `mul8/1' control is the multiplication the LICM does still hoist.
shift_probe_src() ->
    "-module(nsh_probe).\n"
    "-export([shl/1, shl2/2, shr2/2, mul8/1]).\n"
    "shl(T) when is_integer(T), T >= 0 ->\n"
    "    receive stop -> T bsl 3 after 0 -> T end;\n"
    "shl(T) -> T.\n"
    "shl2(T, N) when is_integer(T), is_integer(N) ->\n"
    "    receive stop -> T bsl N after 0 -> T end;\n"
    "shl2(T, _N) -> T.\n"
    "shr2(T, N) when is_integer(T), is_integer(N) ->\n"
    "    receive stop -> T bsr N after 0 -> T end;\n"
    "shr2(T, _N) -> T.\n"
    "mul8(T) when is_integer(T), T >= 0 ->\n"
    "    receive stop -> T * 8 after 0 -> T end;\n"
    "mul8(T) -> T.\n".

%% Call `Mod:F(Args...)' with (`Send') or without a pending `stop' message
%% and return the value or the {Class,Reason} of the exception.
shift_call(Mod, F, Args, Send) ->
    flush(),
    case Send of
        true -> self() ! stop;
        false -> ok
    end,
    R = try {ok, apply(Mod, F, Args)} catch C:R0 -> {C, R0} end,
    flush(),
    R.

%% The largest bit index a bignum of this emulator can have: the largest `N'
%% for which `1 bsl N' succeeds. A shift whose result would have a higher
%% index raises `system_limit'. The index is derived from the emulator's
%% digit size, so it must not be hard-coded.
bsl_limit() ->
    bsl_limit(1, 1 bsl 40).

bsl_limit(Lo, Hi) when Hi - Lo =< 1 ->
    Lo;
bsl_limit(Lo, Hi) ->
    Mid = (Lo + Hi) div 2,
    case try 1 bsl Mid catch error:_E -> overflow end of
        N when is_integer(N) -> bsl_limit(Mid, Hi);
        _ -> bsl_limit(Lo, Mid)
    end.

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