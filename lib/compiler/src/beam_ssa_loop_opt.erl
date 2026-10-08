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
-module(beam_ssa_loop_opt).
-moduledoc false.

%%% Loop optimizations on the BEAM SSA IR.
%%%
%%% Opt-in: the pass runs only with the `loop_opt' compiler option (appended
%%% to the repeated pass list by `beam_ssa_opt:repeated_passes/1'), and can be
%%% switched off with `no_ssa_opt_loop_opt' (auto-derived by
%%% `beam_ssa_opt:passes_1/2') or with `no_ssa_opt'. The separate arithmetic
%%% LICM below has its own option `loop_arith_licm' and its own kill switch
%%% `no_ssa_opt_loop_arith_licm'; `loop_opt' alone never enables it.
%%%
%%% MODULE LAYOUT
%%%
%%% This module is the driver: it owns the two published entry points
%%% (`opt/1' for `loop_opt', `opt_arith/1' for `loop_arith_licm'), the
%%% eligibility gate `eligible/1', loop recognition driving and the base LICM
%%% of items 1-2 below. The supporting machinery lives in three sibling
%%% modules, so that the pieces that are shared with (or independent of) the
%%% loop drive are testable and readable on their own:
%%%
%%% * `beam_ssa_loop_cfg' -- the CFG vocabulary: successors/predecessors,
%%%   dominators, natural-loop recognition, the iteration region of a loop,
%%%   the preheader and its insertion, the RPO reordering, and the cheap
%%%   back-edge gate.
%%% * `beam_ssa_loop_reduce' -- induction-variable canonicalisation, the
%%%   integrality/non-negativity proof (items 3-4), and the whole-function
%%%   strength reduction.
%%% * `beam_ssa_loop_arith' -- the arithmetic LICM of `loop_arith_licm'
%%%   (the second pass), which needs the loop preheader.
%%%
%%% WHAT THE BASE PASS (`loop_opt') DOES
%%%
%%% 1. Loop recognition: natural loops from the CFG (an edge H->P is a back
%%%    edge when H dominates P; the body is {H} plus everything that reaches
%%%    P without passing through H). Loops sharing a header are merged. A
%%%    loop is transformed only when its header has exactly one predecessor
%%%    outside the loop; that block is the preheader, or a fresh preheader is
%%%    inserted on the edge when it has other successors. (All of this is
%%%    `beam_ssa_loop_cfg'.)
%%%
%%% 2. Loop-invariant code motion (LICM) of the constructors in ?SAFE_OPS and
%%%    the tag tests in ?SAFE_BIFS: pure instructions that can never raise for
%%%    any operand values and have a block-independent code generation rule,
%%%    whose operands are all defined outside the loop. They are moved into
%%%    the preheader, i.e. from once per iteration to once per loop entry.
%%%    Arithmetic is NOT hoisted here (it can raise); see `loop_arith_licm'.
%%%
%%% 3. Induction-variable canonicalisation: a loop-carried phi of the header
%%%    `V = phi({Init, Outside}, [{V + C, Inside} | ...])' with one integer
%%%    literal step C (the same on every inside edge, `V' itself meaning 0)
%%%    and a single integer entry value is described by the pair {Init, Step}.
%%%    The recurrence is not rewritten -- `C + V' and `C * V' *are*
%%%    canonicalised to `V + C' and `V * C', which is exact for every number.
%%%    The description only licences the reduction of values computed from
%%%    the induction variable. (In `beam_ssa_loop_reduce'.)
%%%
%%% 4. Strength reduction: an arithmetic instruction is rewritten in place
%%%    (same block, same destination; no instruction is ever moved or
%%%    duplicated) into an exactly equivalent cheaper one, and only when the
%%%    rewrite is exact for every value its operands can have:
%%%
%%%    * `X * 2^n' (n >= 1, `2^n' a literal) becomes `X bsl n' when X is
%%%      provably an integer; for integers, including bignums, `bsl' is
%%%      exactly multiplication by a power of two.
%%%    * `X * 2' becomes `X + X' otherwise, which is exact for every number.
%%%    * `X div 2^n' (n >= 1) becomes `X bsr n' when X is provably a
%%%      NON-NEGATIVE integer (`div' truncates toward zero, `bsr' floors).
%%%    * `X rem 2^n' (n >= 1) becomes `X band (2^n - 1)' when X is provably a
%%%      non-negative integer (`rem' of a negative dividend would not agree).
%%%
%%%    Everything else is left alone: a factor or divisor that is not a
%%%    literal power of two, `X div 0' (which must keep raising), `rem' by a
%%%    power of two with a possibly negative dividend, and every operation
%%%    with an operand whose integrality/non-negativity cannot be proved.
%%%    The reducer is applied to the WHOLE FUNCTION -- every rewrite is local
%%%    and proved at the instruction -- and only IV recognition is loop-scoped.
%%%    (In `beam_ssa_loop_reduce'.)
%%%
%%% INTEGRALITY AND NON-NEGATIVITY PROOF (shared by items 3-4 and by LICM)
%%%
%%% Two independent sources, either of which is enough:
%%%
%%% * the *structural* facts (`beam_ssa_loop_reduce:value_kinds/2' +
%%%   `beam_ssa_loop_reduce:def_kind/4'): an integer literal; a canonical
%%%   induction variable (integer entry value and steps, non-negative as well
%%%   when the entry value and every step are non-negative -- an induction
%%%   over the iterations); a phi whose incoming values are all integers
%%%   (non-negative integers); a `{bif,Op}' with Op in ?INT_BIFS (?NAT_BIFS,
%%%   both in `beam_ssa_loop_reduce') whose operands are all integers
%%%   (non-negative integers). A fact about a value defined outside a loop
%%%   holds on every path into the loop, because the definitions it rests on
%%%   dominate the uses -- which is what makes the structural facts usable at
%%%   a hoist point.
%%% * the *type annotations* `beam_ssa_type' leaves on an instruction
%%%   (`anno = #{arg_types := #{Index => #type{}}}', the type of the operand
%%%   at the 0-based Index): `#t_integer{}' proves integrality and a lower
%%%   element bound >= 0 proves non-negativity. They are PATH-DEPENDENT: the
%%%   analysis refines a variable along the edges it flows down, so the type at
%%%   one use says nothing about another. Used in place by
%%%   `beam_ssa_loop_reduce:operand_kind/4' and, for hoisting, through the
%%%   extra rule of `beam_ssa_loop_arith:pre_kind/5'.
%%%
%%% CORRECTNESS RULES
%%%
%%% * Only `#b_set{}' value instructions are considered; phis, terminators,
%%%   calls and side-effecting instructions are never touched.
%%% * The strength reducer replaces an operation only by one that either
%%%   succeeds and computes the same value or fails on exactly the same
%%%   operands. Every rewrite proves the operands integral (non-negative for
%%%   `div' -> `bsr' and `rem' -> `band'), so the operand set on which the new
%%%   instruction fails is empty and no exception is added. The divisor stays a
%%%   literal power of two, so no division by zero is created or removed.
%%% * The whole transformation is wrapped so that any unexpected condition
%%%   skips the optimization rather than failing the compilation.
%%%
%%% NOT IMPLEMENTED: unrolling (needs an exact trip count); accumulator-based
%%% strength reduction of `X * K' for a non-power-of-two K; `X * 2^n' for an X
%%% that may be a float; a shift by a negative or non-literal count; any
%%% divisor that could be 0. A precise reason for the two whole-loop
%%% transformations that were considered and rejected follows.
%%%
%%% NOT IMPLEMENTED AND WHY: LOOP UNROLLING (sub-flag `loop_unroll', pass
%%% `ssa_opt_loop_unroll'). Unrolling needs an EXACT trip count and a body that
%%% may be duplicated; neither exists here.
%%% * The only CFG cycles that reach this pass are the `receive' loops built by
%%%   `beam_ssa_recv' -- `beam_ssa:is_loop_header/1' recognizes a header only
%%%   by `peek_message'/`wait_timeout', and an ordinary tail-recursive function
%%%   is emitted as a `call_only'/`call_last' tail call with NO cycle, so it
%%%   has no loop to unroll and no induction variable. A `receive' loop has no
%%%   trip count at all: its iteration count is the number of matching
%%%   messages, which is not a compile-time quantity.
%%% * Duplicating a `receive' body is not even semantics-preserving: it would
%%%   run two `peek_message'/`wait_timeout'/`remove_message' rounds before
%%%   re-entering the loop, reordering the mailbox and making the `after'
%%%   timeout apply once per unrolled pair instead of once per message.
%%% A sound unroller therefore could never fire on any Erlang program compiled
%%% through this pipeline, and the counted-loop shape it would need is not
%%% produced by `beam_core_to_ssa'. It was rejected rather than written as
%%% unreachable, unverifiable code. If the pipeline ever grows real counted
%%% loops, the transform belongs in `beam_ssa_opt:repeated_passes/1' mirroring
%%% the `loop_arith_licm' block, as
%%%   Ps3 = case lists:member(loop_unroll, Opts) of
%%%             true -> Ps2 ++ [{ssa_opt_loop_unroll,
%%%                              fun(St) -> beam_ssa_loop_opt:opt_unroll(St) end}];
%%%             false -> Ps2
%%%         end,
%%% with a sound `gate(ssa_opt_loop_unroll)' that accepts both a back edge and
%%% the countable-loop shape.
%%%
%%% NOT IMPLEMENTED AND WHY: LOOP-INDEX NARROWING (sub-flag
%%% `loop_index_narrowing', pass `ssa_opt_loop_index_narrowing'). Narrowing an
%%% index has no meaning on this IR: an Erlang integer is a single
%%% arbitrary-precision type and the BEAM instruction set has no narrow or
%%% wrapping integer form, so there is no cheaper operation to rewrite an index
%%% into. The two rewrites that could pass for it are already done, or belong
%%% elsewhere:
%%% * `X rem 2^n' -> `X band (2^n - 1)' -- the only index "narrowing" that maps
%%%   to a cheaper BEAM instruction -- is already the `rem' rule of
%%%   `beam_ssa_loop_reduce:strength_reduce/2' (and needs a non-negative
%%%   dividend).
%%% * A range-proven index (the JIT's machine-integer win) is produced by the
%%%   SSA TYPE analysis: `beam_ssa_type' already narrows `#t_integer' by lower
%%%   and upper element bounds along the loop edges, and a second, loop-pass
%%%   range inference could only duplicate or contradict it.
%%% And, as for unrolling, the only loops that exist here are `receive' loops,
%%% which have no index to narrow. Rejected for the same reason; the
%%% registration would mirror the block quoted above with the pass name
%%% `ssa_opt_loop_index_narrowing' and the option `loop_index_narrowing'.
%%%
%%% REACH (the pass runs after the SSA type analysis): the CFG of an ordinary
%%% tail-recursive function contains no cycle (self-recursion is still a tail
%%% `call') and a `receive' loop's header carries no phis, so the loop and IV
%%% parts find little to do in typical Erlang code. The strength reducer needs
%%% neither a phi nor an IV, only a proof that an operand is an integer, which
%%% the type annotations provide -- and it is applied to the whole function.
-export([opt/1, opt_arith/1, eligible/1]).

-include("beam_ssa.hrl").
-import_record(beam_ssa_opt, [opt_st]).

%% Maximum number of instructions hoisted per loop. Bounds the growth of the
%% preheader on pathological input. The arithmetic LICM in
%% `beam_ssa_loop_arith' uses the same bound.
-define(MAX_HOIST, 64).

%% Operations that are side-effect free, can never raise for any operand
%% values, and have a block-independent code generation rule. Purity and
%% non-raising are necessary but NOT sufficient: the fused type tests
%% `is_tagged_tuple'/3 and `is_nonempty_list'/1 exist only as block-local tests
%% consumed by the block terminator (`cg_block/3') with no standalone
%% `cg_instr/3' clause, so hoisting one breaks code generation. Constructors are
%% ordinary value instructions and are safe to move.
-define(SAFE_OPS, [put_list, put_tuple]).

%% `{bif,Name}' value instructions that are additionally hoisted, listed as
%% {Name,Arity} pairs. These are the Erlang tag tests: each is a plain tag
%% comparison, pure and non-raising for any argument (see the `is_*_1'/1
%% functions in `erts/emulator/beam/erl_bif_op.c'), and each is a `ubif' in
%% bif.tab, so `{bif,Name,{f,0},Args,Dst}' is a valid standalone instruction as
%% well as a block-local test. The arity is part of the key: `is_function/2'
%% and `is_record/2,3' raise `badarg' and are deliberately absent.
-define(SAFE_BIFS, [{is_atom,1}, {is_binary,1}, {is_bitstring,1},
                    {is_boolean,1}, {is_float,1}, {is_function,1},
                    {is_integer,1}, {is_list,1}, {is_map,1}, {is_number,1},
                    {is_pid,1}, {is_port,1}, {is_reference,1},
                    {is_tuple,1}]).

-spec opt({#opt_st{}, term()}) -> {#opt_st{}, term()}.
opt({#opt_st{ssa=Linear0}=St, FuncDb}) when is_list(Linear0) ->
    wrap(St, FuncDb, fun(Linear) -> transform(Linear, safe) end);
opt({St, FuncDb}) ->
    {St, FuncDb}.

%% The `loop_arith_licm' pass: the arithmetic LICM of
%% `beam_ssa_loop_arith:arith_licm/4'. A separate pass (and therefore a
%% separate kill switch `no_ssa_opt_loop_arith_licm') because hoisting an
%% instruction that can raise is riskier than the rest of the pass and must
%% not be reachable from `loop_opt' alone.
-spec opt_arith({#opt_st{}, term()}) -> {#opt_st{}, term()}.
opt_arith({#opt_st{ssa=Linear0}=St, FuncDb}) when is_list(Linear0) ->
    wrap(St, FuncDb, fun(Linear) -> transform(Linear, arith) end);
opt_arith({St, FuncDb}) ->
    {St, FuncDb}.

wrap(#opt_st{ssa=Linear0}=St, FuncDb, Fun) ->
    %% Any unexpected condition skips the optimization; it must never turn a
    %% compilable module into a compilation error.
    Linear = try
                 Fun(Linear0)
             catch
                 _:_ ->
                     Linear0
             end,
    {St#opt_st{ssa=Linear}, FuncDb}.

%% Eligibility precondition used by the opt-in per-pass budget/watchdog
%% driver (`beam_ssa_opt_budget', wired in `beam_ssa_opt:gate/1'): true when
%% the pass can change anything at all.
%%
%% This is only a NECESSARY condition, a cheap filter that lets the driver
%% skip the pass for the majority of functions; whether a specific loop can
%% be optimized remains the business of the pass itself.
%%
%% The state is a linear (reverse postorder) block list at this point in the
%% pipeline. In reverse postorder every edge of acyclic control flow goes
%% from an earlier to a later block, while every cycle produces at least one
%% edge whose target is not after its source. Testing "some successor is not
%% later than the block itself" is therefore a sound over-approximation of
%% "this function has a loop", and it costs O(V+E) with no dominator
%% computation (`beam_ssa_loop_cfg:has_back_edge/1').
%%
%% A loop is NOT the only thing the `loop_opt' driver rewrites: it also runs a
%% WHOLE-FUNCTION strength reduction
%% (`beam_ssa_loop_reduce:strength_reduce/2') that rewrites `X * 2^n',
%% `X div 2^n' and `X rem 2^n' in place and canonicalises `C + V' / `C * V'
%% into `V + C' / `V * C', none of which needs a back edge. The gate must
%% therefore also accept those shapes, or enabling the gates (`ssa_opt_gates')
%% would silently drop those rewrites and change the generated code. The
%% candidate test below over-approximates the pass's own test (a false
%% positive only means "do not skip").
-spec eligible({#opt_st{}, term()}) -> boolean().
eligible({#opt_st{ssa=Linear}, _FuncDb}) when is_list(Linear) ->
    beam_ssa_loop_cfg:has_back_edge(Linear) orelse has_reduce_candidate(Linear);
eligible(_) ->
    %% The SSA is in block-map form, where opt/1 is a no-op.
    false.

%% The whole-function strength-reduction part of the driver rewrites an
%% instruction of one of the shapes tested here, whatever the surrounding
%% control flow. Each clause mirrors a clause of
%% `beam_ssa_loop_reduce:reduce_instr/2': a `*', `div' or `rem' can become a
%% shift or a mask, and a `+' or `*' written with its literal operand on the
%% left is canonicalised into the variable-first form. The
%% literality/integrality facts that decide whether the rewrite actually
%% happens are left to the pass, so this is deliberately wider than the pass.
has_reduce_candidate([{_L, #b_blk{is=Is}} | Blocks]) ->
    case lists:any(fun reduce_candidate/1, Is) of
        true -> true;
        false -> has_reduce_candidate(Blocks)
    end;
has_reduce_candidate([]) ->
    false.

reduce_candidate(#b_set{op={bif,Op}, args=[_, _]})
  when Op =:= '*'; Op =:= 'div'; Op =:= 'rem' ->
    true;
reduce_candidate(#b_set{op={bif,Op}, args=[#b_literal{}, #b_var{}]})
  when Op =:= '+'; Op =:= '*' ->
    true;
reduce_candidate(_I) ->
    false.

%%% ------------------------------------------------------------------
%%% Driver
%%% ------------------------------------------------------------------

%% `safe' is the base `loop_opt' pass: LICM of the tag tests and constructors
%% plus the whole-function strength reduction. `arith' is the `loop_arith_licm'
%% pass: only the arithmetic LICM (which needs the loop's preheader), no
%% reducer -- a module compiled with both options runs both passes.
transform(Linear0, Mode) ->
    Blocks0 = maps:from_list(Linear0),
    Order0 = [L || {L, _} <- Linear0],
    case Order0 of
        [] ->
            Linear0;
        [_ | _] ->
            {Loops, Doms} = beam_ssa_loop_cfg:find_loops(Order0, Blocks0),
            Next = lists:max([0 | Order0]) + 1,
            %% LICM is per loop: it needs that loop's preheader.
            {Blocks1, _} =
                lists:foldl(fun(Loop, {B, N}) ->
                                    licm_loop(Loop, Order0, Doms, Mode, B, N)
                            end, {Blocks0, Next}, Loops),
            Blocks2 = case Mode of
                          safe ->
                              %% The reducer is local and proved at the
                              %% rewritten instruction and never moves or
                              %% duplicates one, so it applies to the whole
                              %% function, not just to the loop region.
                              beam_ssa_loop_reduce:strength_reduce(Loops, Blocks1);
                          arith ->
                              Blocks1
                      end,
            beam_ssa_loop_cfg:order_linear(Order0, Blocks2)
    end.

%%% ------------------------------------------------------------------
%%% Per-loop LICM
%%% ------------------------------------------------------------------

licm_loop({H, Body}, Order0, Doms, Mode, Blocks0, NextLabel) ->
    BodySet = ordsets:from_list(Body),
    Preds = beam_ssa_loop_cfg:preds(maps:keys(Blocks0), Blocks0),
    Outside = [P || P <- maps:get(H, Preds, []),
                    not ordsets:is_element(P, BodySet)],
    case Outside of
        [Pre] when Pre =/= H, is_map_key(Pre, Blocks0) ->
            {Blocks1, Pre1, Next1} =
                beam_ssa_loop_cfg:ensure_preheader(Pre, H, Blocks0, NextLabel),
            Blocks2 = case Mode of
                          safe ->
                              licm(H, Body, Order0, Pre1, Blocks1);
                          arith ->
                              beam_ssa_loop_arith:arith_licm(
                                beam_ssa_loop_cfg:loop_region(H, Order0, Doms),
                                Order0, Pre1, Blocks1)
                      end,
            {Blocks2, Next1};
        _ ->
            {Blocks0, NextLabel}
    end.

%%% ------------------------------------------------------------------
%%% Loop-invariant code motion
%%% ------------------------------------------------------------------

licm(H, Body, Order0, Pre, Blocks0) ->
    BodySet = ordsets:from_list(Body),
    Inv = beam_ssa_loop_cfg:loop_defs(Body, Blocks0),
    Order = [L || L <- Order0, ordsets:is_element(L, BodySet)],
    %% The header must be processed as well; it can hold invariants.
    Order1 = case lists:member(H, Order) of
                 true -> Order;
                 false -> [H | Order]
             end,
    {Blocks, _} =
        lists:foldl(fun(L, {B, Inv0}) ->
                            hoist_block(L, Pre, Inv0, B)
                    end, {Blocks0, Inv}, Order1),
    Blocks.

hoist_block(L, Pre, Inv0, Blocks0) ->
    Blk0 = maps:get(L, Blocks0),
    {Keep, Hoist, Inv} =
        split_is(Blk0#b_blk.is, Inv0, 0, [], []),
    case Hoist of
        [] ->
            {Blocks0, Inv};
        [_ | _] ->
            Blocks1 = Blocks0#{L := Blk0#b_blk{is=Keep}},
            PreBlk0 = maps:get(Pre, Blocks1),
            PreBlk = PreBlk0#b_blk{is=PreBlk0#b_blk.is ++ Hoist},
            {Blocks1#{Pre := PreBlk}, Inv}
    end.

split_is([], Inv, _Cnt, Keep, Hoist) ->
    {lists:reverse(Keep), lists:reverse(Hoist), Inv};
split_is([S | Rest], Inv, Cnt, Keep, Hoist) ->
    case Cnt < ?MAX_HOIST andalso hoistable(S, Inv) of
        true ->
            #b_set{dst=V} = S,
            split_is(Rest, ordsets:del_element(V, Inv), Cnt + 1,
                     Keep, [S | Hoist]);
        false ->
            split_is(Rest, Inv, Cnt, [S | Keep], Hoist)
    end.

hoistable(#b_set{op=phi}, _Inv) ->
    false;
hoistable(#b_set{op=Op, args=Args}, Inv) ->
    safe_op(Op, length(Args)) andalso
        lists:all(fun(A) -> invariant_arg(A, Inv) end, Args).

%% safe_op(Op, Arity) -> boolean().
%%  Is an instruction with this operation (and this number of operands)
%%  pure, non-raising and code-generatable outside its original block?
safe_op(Op, _Arity) when is_atom(Op) ->
    lists:member(Op, ?SAFE_OPS);
safe_op({bif,Name}, Arity) ->
    lists:member({Name, Arity}, ?SAFE_BIFS);
safe_op(_Op, _Arity) ->
    false.
invariant_arg(#b_literal{}, _Inv) ->
    true;
invariant_arg(#b_var{}=V, Inv) ->
    not ordsets:is_element(V, Inv);
invariant_arg(_, _) ->
    false.
