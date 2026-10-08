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
%%% WHAT THE BASE PASS (`loop_opt') DOES
%%%
%%% 1. Loop recognition: natural loops from the CFG (an edge H->P is a back
%%%    edge when H dominates P; the body is {H} plus everything that reaches
%%%    P without passing through H). Loops sharing a header are merged. A
%%%    loop is transformed only when its header has exactly one predecessor
%%%    outside the loop; that block is the preheader, or a fresh preheader is
%%%    inserted on the edge when it has other successors.
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
%%%    the induction variable.
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
%%%
%%% INTEGRALITY AND NON-NEGATIVITY PROOF (shared by items 3-4 and by LICM)
%%%
%%% Two independent sources, either of which is enough:
%%%
%%% * the *structural* facts (`value_kinds/2' + `def_kind/4'): an integer
%%%   literal; a canonical induction variable (integer entry value and steps,
%%%   non-negative as well when the entry value and every step are non-negative
%%%   -- an induction over the iterations); a phi whose incoming values are all
%%%   integers (non-negative integers); a `{bif,Op}' with Op in ?INT_BIFS
%%%   (?NAT_BIFS) whose operands are all integers (non-negative integers). A
%%%   fact about a value defined outside a loop holds on every path into the
%%%   loop, because the definitions it rests on dominate the uses -- which is
%%%   what makes the structural facts usable at a hoist point.
%%% * the *type annotations* `beam_ssa_type' leaves on an instruction
%%%   (`anno = #{arg_types := #{Index => #type{}}}', the type of the operand
%%%   at the 0-based Index): `#t_integer{}' proves integrality and a lower
%%%   element bound >= 0 proves non-negativity. They are PATH-DEPENDENT: the
%%%   analysis refines a variable along the edges it flows down, so the type at
%%%   one use says nothing about another. Used in place by `operand_kind/4'
%%%   and, for hoisting, through the extra rule of `pre_kind/5'.
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
%%% divisor that could be 0.
%%%
%%% REACH (the pass runs after the SSA type analysis): the CFG of an ordinary
%%% tail-recursive function contains no cycle (self-recursion is still a tail
%%% `call') and a `receive' loop's header carries no phis, so the loop and IV
%%% parts find little to do in typical Erlang code. The strength reducer needs
%%% neither a phi nor an IV, only a proof that an operand is an integer, which
%%% the type annotations provide -- and it is applied to the whole function.
-export([opt/1, opt_arith/1, eligible/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").
-import_record(beam_ssa_opt, [opt_st]).

%% Maximum number of instructions hoisted per loop. Bounds the growth of the
%% preheader on pathological input.
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

%% Arithmetic BIFs whose result is an integer whenever all their operands are
%% integers (the operand facts are only claimed for values that were produced,
%% so what `trunc'/`round' do with a non-number does not matter).
-define(INT_BIFS, ['+','-','*','div','rem','band','bor','bxor','bsl','bsr',
                   'abs','trunc','round']).

%% The subset of ?INT_BIFS that also preserves non-negativity: the result is
%% >= 0 whenever every operand is >= 0. Subtraction is absent (it can go
%% negative), as are float-producing operations.
-define(NAT_BIFS, ['+','*','div','rem','band','bor','bxor','bsl','bsr','abs']).

%% The subset of ?INT_BIFS that the `loop_arith_licm' pass may hoist out of a
%% loop. Unlike the tag tests they can raise `badarith', so the pass has to
%% prove them non-raising first; see `arith_non_raising/5'.
-define(ARITH_BIFS, ['+','-','*','div','rem','band','bor','bxor','bsl','bsr']).

%% `{bif,Name}' operations through which the SSA type analysis can narrow one
%% of their arguments (`infer_type'/4 and `infer_relop'/3' in
%% `beam_ssa_type'). A variable tested by one of these inside a loop has a
%% loop-dependent type at its uses; see `narrowed_vars/3'.
-define(NARROW_BIFS, [is_atom, is_binary, is_bitstring, is_boolean, is_float,
                      is_function, is_integer, is_list, is_map, is_number,
                      is_pid, is_port, is_reference, is_tuple, is_record,
                      '==', '/=', '=<', '<', '>=', '>', '=:=', '=/=']).

%% The largest literal shift count the arithmetic LICM accepts. `bsl' raises
%% `system_limit', not `badarith', when the result would exceed the bignum
%% arity bound (`erl_arith.c': `if (ires-8 > BIG_ARITY_MAX)'), so a count that
%% is merely proven non-negative does not make the instruction non-raising.
-define(MAX_LITERAL_SHIFT, 64).

-spec opt({#opt_st{}, term()}) -> {#opt_st{}, term()}.
opt({#opt_st{ssa=Linear0}=St, FuncDb}) when is_list(Linear0) ->
    wrap(St, FuncDb, fun(Linear) -> transform(Linear, safe) end);
opt({St, FuncDb}) ->
    {St, FuncDb}.

%% The `loop_arith_licm' pass: the arithmetic LICM of `arith_licm/6'. A
%% separate pass (and therefore a separate kill switch
%% `no_ssa_opt_loop_arith_licm') because hoisting an instruction that can
%% raise is riskier than the rest of the pass and must not be reachable from
%% `loop_opt' alone.
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
%% the function's CFG contains a cycle, i.e. when there can be a loop to
%% optimize.
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
%% computation.
-spec eligible({#opt_st{}, term()}) -> boolean().
eligible({#opt_st{ssa=Linear}, _FuncDb}) when is_list(Linear) ->
    has_back_edge(Linear);
eligible(_) ->
    %% The SSA is in block-map form, where opt/1 is a no-op.
    false.

has_back_edge(Linear) ->
    Index = index_blocks(Linear, #{}, 0),
    scan_edges(Linear, Index).

index_blocks([{L,_}|Blocks], Map, I) ->
    index_blocks(Blocks, Map#{L => I}, I + 1);
index_blocks([], Map, _I) ->
    Map.

scan_edges([{L,#b_blk{last=Last}}|Blocks], Index) ->
    case has_back_succ(succs_last(Last), map_get(L, Index), Index) of
        true -> true;
        false -> scan_edges(Blocks, Index)
    end;
scan_edges([], _Index) ->
    false.

has_back_succ([S|Ss], I, Index) ->
    case Index of
        #{S := SI} when SI =< I -> true;
        _ -> has_back_succ(Ss, I, Index)
    end;
has_back_succ([], _I, _Index) ->
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
            {Loops, Doms} = find_loops(Order0, Blocks0),
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
                              strength_reduce(Loops, Blocks1);
                          arith ->
                              Blocks1
                      end,
            order_linear(Order0, Blocks2)
    end.

%% Preserve the original (RPO) block order and append freshly created
%% preheader blocks at the end.
order_linear(Order0, Blocks) ->
    New = lists:sort([L || L <- maps:keys(Blocks),
                           not lists:member(L, Order0)]),
    [{L, maps:get(L, Blocks)} || L <- Order0 ++ New].

%%% ------------------------------------------------------------------
%%% Loop recognition
%%% ------------------------------------------------------------------

find_loops(Labels, Blocks) ->
    Preds = preds(Labels, Blocks),
    Doms = dominators(Labels, Blocks),
    Edges = [{H, P} || H <- Labels,
                       P <- maps:get(H, Preds, []),
                       lists:member(H, maps:get(P, Doms, []))],
    Loops = lists:foldl(
              fun({H, P}, Acc) ->
                      Body = natural_loop(H, P, Preds),
                      case lists:keyfind(H, 1, Acc) of
                          false ->
                              [{H, Body} | Acc];
                          {H, Body0} ->
                              lists:keyreplace(H, 1, Acc,
                                               {H, ordsets:union(Body0, Body)})
                      end
              end, [], Edges),
    {[{H, ordsets:to_list(Body)} || {H, Body} <- Loops], Doms}.
%% The natural loop of the back edge H->P: H plus every block that can reach
%% P without passing through H.
natural_loop(H, P, Preds) ->
    nl_walk([P], ordsets:from_list([H, P]), Preds).

nl_walk([], Acc, _Preds) ->
    Acc;
nl_walk([N | Work], Acc0, Preds) ->
    {Acc, Work1} =
        lists:foldl(fun(Pr, {A, W}) ->
                            case ordsets:is_element(Pr, A) of
                                true -> {A, W};
                                false -> {ordsets:add_element(Pr, A), [Pr | W]}
                            end
                    end, {Acc0, Work}, maps:get(N, Preds, [])),
    nl_walk(Work1, Acc, Preds).

preds(Labels, Blocks) ->
    lists:foldl(
      fun(L, Acc) ->
              Ss = succs(maps:get(L, Blocks)),
              lists:foldl(fun(S, A) ->
                                  maps:update_with(
                                    S, fun(Ls) -> [L | Ls] end, [L], A)
                          end, Acc, Ss)
      end, #{L => [] || L <- Labels}, Labels).

succs(#b_blk{last=Last}) ->
    succs_last(Last).

succs_last(#b_br{succ=S, fail=F}) when S =:= F -> [S];
succs_last(#b_br{succ=S, fail=F}) -> [S, F];
succs_last(#b_switch{fail=F, list=Ls}) ->
    lists:usort([F | [L || {_, L} <- Ls]]);
succs_last(#b_ret{}) ->
    [].

%% Iterative dominator computation. Returns #{Label => [Dominator]} where the
%% list includes the label itself.
dominators(Labels, Blocks) ->
    Preds = preds(Labels, Blocks),
    [Entry | _] = Labels,
    Init = maps:from_list(
             [{L, case L =:= Entry of
                      true -> [L];
                      false -> Labels
                  end} || L <- Labels]),
    dom_fix(Labels, Preds, Init).

dom_fix(Labels, Preds, Doms) ->
    Doms1 = lists:foldl(
              fun(L, D) ->
                      Ps = [P || P <- maps:get(L, Preds, []),
                                 maps:is_key(P, D)],
                      case Ps of
                          [] ->
                              D;
                          [_ | _] ->
                              Inter = lists:foldl(
                                        fun(P, Acc) ->
                                                case Acc of
                                                    none -> maps:get(P, D);
                                                    _ -> ordsets:intersection(
                                                           Acc, maps:get(P, D))
                                                end
                                        end, none, Ps),
                              New = ordsets:union([L], Inter),
                              case New =:= maps:get(L, D) of
                                  true -> D;
                                  false -> D#{L := New}
                              end
                      end
              end, Doms, Labels),
    case Doms1 =:= Doms of
        true -> Doms1;
        false -> dom_fix(Labels, Preds, Doms1)
    end.

%%% ------------------------------------------------------------------
%%% Per-loop LICM
%%% ------------------------------------------------------------------

licm_loop({H, Body}, Order0, Doms, Mode, Blocks0, NextLabel) ->
    BodySet = ordsets:from_list(Body),
    Preds = preds(maps:keys(Blocks0), Blocks0),
    Outside = [P || P <- maps:get(H, Preds, []),
                    not ordsets:is_element(P, BodySet)],
    case Outside of
        [Pre] when Pre =/= H, is_map_key(Pre, Blocks0) ->
            {Blocks1, Pre1, Next1} = ensure_preheader(Pre, H, Blocks0, NextLabel),
            Blocks2 = case Mode of
                          safe ->
                              licm(H, Body, Order0, Pre1, Blocks1);
                          arith ->
                              arith_licm(loop_region(H, Order0, Doms),
                                         Order0, Pre1, Blocks1)
                      end,
            {Blocks2, Next1};
        _ ->
            {Blocks0, NextLabel}
    end.

%% The blocks inside a loop's iteration region: the body of the natural loop
%% plus every block the loop header dominates.
%%
%% The natural-loop body is "the blocks that can reach the back edge without
%% passing through the header", which is too narrow here: in a `receive' loop
%% the block that computes the value of a matching clause ends in `ret' (or
%% branches to the raise block), so it cannot reach the back edge and is not
%% part of any natural loop -- yet it runs once per matching message and is
%% exactly where the arithmetic of the clause lives. A block dominated by the
%% header is inside the iteration region: control only gets there through the
%% header, so it is executed at most once per iteration. The header dominates
%% every block of its natural loop, so the region contains the body and the
%% preheader dominates every block in it.
loop_region(H, Order0, Doms) ->
    [L || L <- Order0, lists:member(H, maps:get(L, Doms, []))].
%% Make sure `Pre' is a dedicated preheader of `H'. If `Pre' has no other
%% successor than `H' it already is one. Otherwise insert a fresh block on the
%% Pre->H edge and redirect the header's phi arguments coming from `Pre'.
ensure_preheader(Pre, H, Blocks, Next) ->
    case [S || S <- succs(maps:get(Pre, Blocks)), S =/= H] of
        [] ->
            {Blocks, Pre, Next};
        _ ->
            Blk = #b_blk{is=[],
                         last=#b_br{bool=#b_literal{val=true}, succ=H, fail=H}},
            PreBlk0 = maps:get(Pre, Blocks),
            PreBlk = PreBlk0#b_blk{last=redirect(PreBlk0#b_blk.last, H, Next)},
            HBlk0 = maps:get(H, Blocks),
            HBlk = HBlk0#b_blk{is=retarget_phis(HBlk0#b_blk.is, Pre, Next)},
            {Blocks#{Pre := PreBlk, H := HBlk, Next => Blk}, Next, Next + 1}
    end.

redirect(Last, H, New) ->
    case Last of
        #b_br{succ=S, fail=F} ->
            Last#b_br{succ=retarget(S, H, New), fail=retarget(F, H, New)};
        #b_switch{fail=F, list=Ls} ->
            Last#b_switch{fail=retarget(F, H, New),
                          list=[{V, retarget(L, H, New)} || {V, L} <- Ls]};
        _ ->
            Last
    end.

retarget(L, L, New) -> New;
retarget(L, _, _) -> L.

retarget_phis([#b_set{op=phi, args=Args}=Phi | Is], From, To) ->
    Args1 = [{V, case L of From -> To; _ -> L end} || {V, L} <- Args],
    [Phi#b_set{args=Args1} | retarget_phis(Is, From, To)];
retarget_phis([I | Is], From, To) ->
    [I | retarget_phis(Is, From, To)];
retarget_phis([], _, _) ->
    [].

%%% ------------------------------------------------------------------
%%% Loop-invariant code motion
%%% ------------------------------------------------------------------

licm(H, Body, Order0, Pre, Blocks0) ->
    BodySet = ordsets:from_list(Body),
    Inv = loop_defs(Body, Blocks0),
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

loop_defs(Body, Blocks) ->
    lists:foldl(
      fun(L, Acc) ->
              Is = (maps:get(L, Blocks))#b_blk.is,
              lists:foldl(fun(#b_set{dst=V}, A) ->
                                  ordsets:add_element(V, A)
                          end, Acc, Is)
      end, ordsets:new(), Body).

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

%%% ------------------------------------------------------------------
%%% Loop-invariant code motion of arithmetic (`loop_arith_licm')
%%% ------------------------------------------------------------------
%%%
%%% `+/'-/*' raise `badarith' for a non-number operand, `div'/`rem' for a zero
%%% divisor too, `band'/`bor'/`bxor' for a non-integer, and `bsl'/`bsr' for a
%%% non-integer, a negative count or a `system_limit' overflow, so hoisting one
%%% needs a proof that it cannot raise AT THE PREHEADER (see below).
%%%
%%% A structural fact is valid there: it rests on definitions that dominate the
%%% preheader. A type annotation is NOT: the type analysis refines a variable
%%% along the edges it flows down, so a variable that the loop narrows with a
%%% test or a comparison can be an integer at that use but need not be one at
%%% the preheader. An annotation is therefore trusted only for an operand that
%%% the loop does not narrow (`narrowed_vars/3').

arith_licm(Region, Order0, Pre, Blocks0) ->
    RegionSet = ordsets:from_list(Region),
    InRegion = loop_defs(Region, Blocks0),
    Defs = defs_map(Blocks0),
    Kinds = value_kinds(Defs, #{}),
    Narrowed = narrowed_vars(Region, Blocks0, Defs),
    Order = [L || L <- Order0, ordsets:is_element(L, RegionSet)],
    lists:foldl(fun(L, B) ->
                        arith_hoist_block(L, Pre, InRegion, Kinds, Narrowed, B)
                end, Blocks0, Order).

arith_hoist_block(L, Pre, InRegion, Kinds, Narrowed, Blocks0) ->
    Blk0 = maps:get(L, Blocks0),
    {Keep, Hoist} =
        arith_split(Blk0#b_blk.is, InRegion, Kinds, Narrowed, 0, [], []),
    case Hoist of
        [] ->
            Blocks0;
        [_ | _] ->
            Blocks1 = Blocks0#{L := Blk0#b_blk{is=Keep}},
            PreBlk0 = maps:get(Pre, Blocks1),
            PreBlk = PreBlk0#b_blk{is=PreBlk0#b_blk.is ++ Hoist},
            Blocks1#{Pre := PreBlk}
    end.

%% The region's definition set is never shrunk: an instruction hoisted earlier
%% in this pass has a definition inside the region, and classifying it would
%% rest on annotations that the loop may narrow. Chains of hoisted arithmetic
%% are therefore left in the loop.
arith_split([], _InRegion, _Kinds, _Narrowed, _Cnt, Keep, Hoist) ->
    {lists:reverse(Keep), lists:reverse(Hoist)};
arith_split([S | Rest], InRegion, Kinds, Narrowed, Cnt, Keep, Hoist) ->
    case Cnt < ?MAX_HOIST andalso
        arith_hoistable(S, InRegion, Kinds, Narrowed) of
        true ->
            arith_split(Rest, InRegion, Kinds, Narrowed, Cnt + 1,
                        Keep, [S | Hoist]);
        false ->
            arith_split(Rest, InRegion, Kinds, Narrowed, Cnt,
                        [S | Keep], Hoist)
    end.

arith_hoistable(#b_set{op={bif,Op}, args=[A, B]}=I, InRegion, Kinds, Narrowed) ->
    lists:member(Op, ?ARITH_BIFS) andalso
        invariant_arith_arg(A, InRegion) andalso
        invariant_arith_arg(B, InRegion) andalso
        is_integer_kind(pre_kind(I, 0, A, Narrowed, Kinds)) andalso
        is_integer_kind(pre_kind(I, 1, B, Narrowed, Kinds)) andalso
        second_operand_ok(Op, B);
arith_hoistable(_I, _InRegion, _Kinds, _Narrowed) ->
    false.

invariant_arith_arg(#b_literal{}, _InRegion) ->
    true;
invariant_arith_arg(#b_var{}=V, InRegion) ->
    not ordsets:is_element(V, InRegion);
invariant_arith_arg(_Arg, _InRegion) ->
    false.

is_integer_kind(nat) -> true;
is_integer_kind(int) -> true;
is_integer_kind(unknown) -> false.

%% What the second operand has to contribute to "cannot raise". The operations
%% that accept every integer contribute nothing further; `div'/`rem' need a
%% non-zero literal, because a divisor that is a loop-invariant *variable* (or
%% the literal 0) could be zero on the new paths and would have to keep
%% raising there; `bsl'/`bsr' need a literal count in 0..?MAX_LITERAL_SHIFT.
second_operand_ok(Op, B) when Op =:= 'div'; Op =:= 'rem' ->
    is_nonzero_literal(B);
second_operand_ok(Op, #b_literal{val=N}) when Op =:= 'bsl'; Op =:= 'bsr' ->
    is_integer(N) andalso N >= 0 andalso N =< ?MAX_LITERAL_SHIFT;
second_operand_ok(_Op, _B) ->
    true.

is_nonzero_literal(#b_literal{val=N}) when is_integer(N), N =/= 0 -> true;
is_nonzero_literal(_B) -> false.

%% The fact about the operand at the 0-based `Index' of `I' that is valid AT
%% THE PREHEADER of the loop. The structural fixed point is consulted first
%% because it is a fact about the value, and its proof only uses definitions
%% that dominate the preheader. Only when it cannot classify the operand is
%% the annotation at the use consulted, and then only for an operand that the
%% loop does not narrow.
pre_kind(_I, _Index, #b_literal{val=N}, _Narrowed, _Kinds)
  when is_integer(N), N >= 0 ->
    nat;
pre_kind(_I, _Index, #b_literal{val=N}, _Narrowed, _Kinds) when is_integer(N) ->
    int;
pre_kind(I, Index, #b_var{}=V, Narrowed, Kinds) ->
    case value_kind(V, Kinds) of
        unknown ->
            case ordsets:is_element(V, Narrowed) of
                true -> unknown;
                false -> anno_kind(I, Index)
            end;
        Kind ->
            Kind
    end;
pre_kind(_I, _Index, _Arg, _Narrowed, _Kinds) ->
    unknown.

%% The fact proven by the `arg_types' annotation of operand `Index', with no
%% structural fallback (unlike `operand_kind/4').
anno_kind(#b_set{anno=Anno}, Index) ->
    case Anno of
        #{arg_types := #{Index := Type}} -> kind_of_type(Type);
        #{} -> unknown
    end.

%% The variables whose type the SSA type analysis can narrow *inside this
%% region*: an argument of a type test or a comparison, the argument of a
%% `switch', and -- conservatively -- every variable an operand of the
%% instruction that defines a branch condition (whatever that instruction
%% is). For those an annotation at a use is loop-dependent and must not be
%% used at the preheader. The set mirrors the `infer_type/4' clauses of
%% `beam_ssa_type' that refine one of their arguments.
narrowed_vars(Region, Blocks, Defs) ->
    lists:foldl(
      fun(L, Acc0) ->
              #b_blk{is=Is, last=Last} = maps:get(L, Blocks),
              Acc = lists:foldl(fun narrowed_is/2, Acc0, Is),
              narrowed_last(Last, Acc, Defs)
      end, ordsets:new(), Region).

narrowed_is(#b_set{op=Op, args=Args}, Acc) ->
    case is_narrowing(Op) of
        true -> add_vars(Args, Acc);
        false -> Acc
    end;
narrowed_is(_I, Acc) ->
    Acc.

narrowed_last(#b_switch{arg=Arg}, Acc, _Defs) ->
    add_var(Arg, Acc);
narrowed_last(#b_br{bool=#b_var{}=B}, Acc, Defs) ->
    case Defs of
        #{B := #b_set{args=Args}} -> add_vars(Args, Acc);
        #{} -> Acc
    end;
narrowed_last(_Last, Acc, _Defs) ->
    Acc.

is_narrowing({bif,Name}) -> lists:member(Name, ?NARROW_BIFS);
is_narrowing(is_tagged_tuple) -> true;
is_narrowing(is_nonempty_list) -> true;
is_narrowing(_Op) -> false.

add_vars([A | As], Acc) -> add_vars(As, add_var(A, Acc));
add_vars([], Acc) -> Acc.

add_var(#b_var{}=V, Acc) -> ordsets:add_element(V, Acc);
add_var(_Arg, Acc) -> Acc.

%%% ------------------------------------------------------------------
%%% Induction-variable canonicalization and strength reduction
%%% ------------------------------------------------------------------
%%%
%%% Two facts are derived over the SSA of a function, both stated for a value
%%% and only ever claimed for a value that the instruction actually produces:
%%%
%%%   integer(V)      -- whenever a use of V is reached, V is an integer
%%%   non_negative(V) -- whenever a use of V is reached, V is an integer >= 0
%%%
%%% non_negative(V) implies integer(V). They are computed by a monotone fixed
%%% point that starts from "unknown" everywhere, so a fact is only derived when
%%% every rule that derives it has been established; the rules are exactly the
%%% ones listed in the INTEGRALITY section of the moduledoc above.
%%%
%%% TRAP: a function argument, a value lifted out of the mailbox or any other
%%% value with no definition in the function is unknown to this structural
%%% analysis and must NOT be guessed to be an integer because it often is one
%%% at run time; only the type annotation at the using instruction is
%%% admissible for such a value, and it is consulted directly at that
%%% instruction.
%% The reducer is applied to EVERY block of the function (a rewrite is proved
%% at the instruction being rewritten and nothing is moved or duplicated);
%% only the induction variables, which licence `div' -> `bsr' and `rem' ->
%% `band', are loop-scoped -- an IV is defined by, and used only in blocks
%% dominated by, its header, so its fact never reaches a block outside.
strength_reduce(Loops, Blocks0) ->
    Defs = defs_map(Blocks0),
    %% The induction variables are recognised using only the facts that hold
    %% without them (an integer literal entry value, or a variable proven
    %% integer on its own); the facts their canonical description yields are
    %% then fed into the second fixed point, which the rewrites use.
    Kinds0 = value_kinds(Defs, #{}),
    Ivs = lists:foldl(fun({H, Body}, Acc) ->
                              maps:merge(Acc, canonical_ivs(H, ordsets:from_list(Body),
                                                            Defs, Kinds0, Blocks0))
                      end, #{}, Loops),
    Kinds = value_kinds(Defs, Ivs),
    lists:foldl(fun(L, B) ->
                        reduce_block(L, Kinds, B)
                end, Blocks0, maps:keys(Blocks0)).
%% The definition of every variable of the function, used to classify the
%% operands of the instructions inside the loop.
defs_map(Blocks) ->
    maps:fold(
      fun(_L, #b_blk{is=Is}, Acc) ->
              lists:foldl(fun(#b_set{dst=#b_var{}=V}=I, A) -> A#{V => I};
                             (_I, A) -> A
                          end, Acc, Is)
      end, #{}, Blocks).

%% value_kinds(Defs, Ivs) -> #{Var => 'nat' | 'int' | 'unknown'}.
%%  Least fixed point of the value facts. Keys are the variables of the
%%  function; a variable is mapped to `unknown' when neither fact could be
%%  established.
value_kinds(Defs, Ivs) ->
    Vars = maps:keys(Defs),
    Initial = maps:from_list([{V, unknown} || V <- Vars]),
    kinds_fix(Vars, Defs, Ivs, Initial).

kinds_fix(Vars, Defs, Ivs, Kinds0) ->
    Kinds1 = lists:foldl(fun(V, K) ->
                                 K#{V => def_kind(V, Defs, Ivs, K)}
                         end, Kinds0, Vars),
    case Kinds1 =:= Kinds0 of
        true -> Kinds1;
        false -> kinds_fix(Vars, Defs, Ivs, Kinds1)
    end.

def_kind(V, Defs, Ivs, Kinds) ->
    case Ivs of
        #{V := {Init, Step}} ->
            iv_kind(Init, Step, Kinds);
        #{} ->
            case Defs of
                #{V := Def} -> def_kind_1(Def, Kinds);
                #{} -> unknown
            end
    end.

def_kind_1(#b_set{op=phi, args=Args}, Kinds) ->
    join_kinds([value_kind(V, Kinds) || {V, _L} <- Args]);
def_kind_1(#b_set{op={bif,Op}, args=Args}=Def, Kinds) ->
    %% The operands are classified at their use, so the type annotations on
    %% this defining instruction count as well. That is what makes a value
    %% computed from a function argument or from a value extracted out of a
    %% term -- neither of which has a defining instruction of its own -- a
    %% proven integer.
    Ks = operand_kinds(Def, Args, 0, Kinds),
    case lists:member(Op, ?NAT_BIFS) andalso all_nat(Ks) of
        true -> nat;
        false ->
            case lists:member(Op, ?INT_BIFS) andalso all_int(Ks) of
                true -> int;
                false -> unknown
            end
    end;
def_kind_1(_Def, _Kinds) ->
    unknown.
%% The fact for a canonical induction variable: an integer entry value and
%% integer steps make it an integer, and a non-negative entry value with
%% non-negative steps makes it non-negative. A decreasing induction variable
%% is still an integer, but can go negative, so only the integer fact is used.
iv_kind(Init, Step, Kinds) ->
    case value_kind(Init, Kinds) of
        unknown -> unknown;
        int -> int;
        nat when Step >= 0 -> nat;
        nat -> int
    end.

value_kind(#b_literal{val=N}, _Kinds) when is_integer(N), N >= 0 ->
    nat;
value_kind(#b_literal{val=N}, _Kinds) when is_integer(N) ->
    int;
value_kind(#b_var{}=V, Kinds) ->
    maps:get(V, Kinds, unknown);
value_kind(_Value, _Kinds) ->
    unknown.

%% The fact an SSA type annotation proves, if any.
%%
%% Only an integer type proves integrality: `any', a float, a number (which
%% may be a float), a union with a non-integer member, a tuple, ... prove
%% nothing and yield `unknown'. A range whose lower bound is a non-integer
%% (`'-inf'') or a negative number proves integrality but not
%% non-negativity, so it yields `int'.
kind_of_type(#t_integer{elements=any}) ->
    int;
kind_of_type(#t_integer{elements={Lo, _}}) when is_integer(Lo), Lo >= 0 ->
    nat;
kind_of_type(#t_integer{}) ->
    int;
kind_of_type(_Type) ->
    unknown.

%% The kind of the operand at the 0-based `Index' of instruction `I'.
%%
%% The type annotation on `I' is authoritative: it is the type the SSA type
%% analysis proved for that operand *at this instruction*. It is consulted
%% first, and the structural fixed point (`value_kinds/2') is the fallback
%% for operands the analysis left unannotated, and for the synthetic SSA the
%% whitebox tests drive the pass with.
%%
%% A variable's type must not be taken from an annotation on a *different*
%% instruction: the type analysis refines a variable along the edges it
%% flows down, so it can be an integer at one use and a float at another.
operand_kind(#b_set{anno=Anno}, Index, Arg, Kinds) ->
    case Anno of
        #{arg_types := #{Index := Type}} ->
            case kind_of_type(Type) of
                unknown -> value_kind(Arg, Kinds);
                Kind -> Kind
            end;
        #{} ->
            value_kind(Arg, Kinds)
    end.

%% The kinds of all operands of `I', in order.
operand_kinds(#b_set{}=I, [Arg | Args], Index, Kinds) ->
    [operand_kind(I, Index, Arg, Kinds) | operand_kinds(I, Args, Index + 1, Kinds)];
operand_kinds(_I, [], _Index, _Kinds) ->
    [].
all_int([]) -> false;
all_int(Ks) -> lists:all(fun(K) -> K =:= int orelse K =:= nat end, Ks).

all_nat([]) -> false;
all_nat(Ks) -> lists:all(fun(K) -> K =:= nat end, Ks).

join_kinds([]) -> unknown;
join_kinds(Ks) ->
    case all_int(Ks) of
        true ->
            case all_nat(Ks) of
                true -> nat;
                false -> int
            end;
        false ->
            unknown
    end.

%% The canonical induction variables of the loop headed by H, i.e. the phis
%% of H of the shape
%%
%%     V = phi({Init, Outside}, [{V + C, Inside} | ...])
%%
%% with an integer literal step C (the same on every inside edge, `V' itself
%% meaning 0) and integer entry values. Returns #{Var => {Init, Step}}.
canonical_ivs(H, BodySet, Defs, Kinds, Blocks) ->
    #b_blk{is=Is} = maps:get(H, Blocks),
    lists:foldl(fun(#b_set{op=phi, dst=#b_var{}=V, args=Args}, Acc) ->
                        case iv_of(V, Args, BodySet, Defs, Kinds) of
                            {ok, Iv} -> Acc#{V => Iv};
                            no -> Acc
                        end;
                   (_I, Acc) ->
                        Acc
                end, #{}, Is).

iv_of(V, Args, BodySet, Defs, Kinds) ->
    Outside = [Val || {Val, L} <- Args, not ordsets:is_element(L, BodySet)],
    Inside = [Val || {Val, L} <- Args, ordsets:is_element(L, BodySet)],
    case Outside =/= [] andalso Inside =/= [] andalso
        lists:all(fun(Val) -> integer_value(Val, Kinds) end, Outside) of
        true ->
            case steps(Inside, V, Defs) of
                {ok, Step} ->
                    %% A loop this pass transforms has exactly one
                    %% predecessor outside the body (its preheader), so the
                    %% entry value is unambiguous; if there are several, they
                    %% must at least all provide the same value.
                    case lists:usort(Outside) of
                        [Init] -> {ok, {Init, Step}};
                        _ -> no
                    end;
                no ->
                    no
            end;
        false ->
            no
    end.

%% The step of every inside edge must be the same integer literal. A single
%% inside edge that is not a `V +/- C' recurrence (step/3 returns `no') makes
%% the variable non-canonical, because its value there is then unknown.
steps([_|_]=Inside, V, Defs) ->
    case lists:map(fun(Val) -> step(Val, V, Defs) end, Inside) of
        [{ok, Step} | Rest] ->
            case lists:all(fun(S) -> S =:= {ok, Step} end, Rest) of
                true -> {ok, Step};
                false -> no
            end;
        [_ | _] ->
            no
    end;
steps([], _V, _Defs) ->
    %% Not loop-carried at all: no induction variable.
    no.

step(#b_var{name=Name}, #b_var{name=Name}, _Defs) ->
    {ok, 0};
step(#b_var{}=W, V, Defs) ->
    case Defs of
        #{W := #b_set{op={bif,Op}, args=Args}} -> bif_step(Op, Args, V);
        #{} -> no
    end;
step(_Val, _V, _Defs) ->
    no.

bif_step('+', Args, V) ->
    case Args of
        [V, #b_literal{val=C}] when is_integer(C) -> {ok, C};
        [#b_literal{val=C}, V] when is_integer(C) -> {ok, C};
        _ -> no
    end;
bif_step('-', Args, V) ->
    case Args of
        [V, #b_literal{val=C}] when is_integer(C) -> {ok, -C};
        _ -> no
    end;
bif_step(_Op, _Args, _V) ->
    no.

integer_value(#b_literal{val=N}, _Kinds) ->
    is_integer(N);
integer_value(#b_var{}=V, Kinds) ->
    case maps:get(V, Kinds, unknown) of
        nat -> true;
        int -> true;
        unknown -> false
    end;
integer_value(_Value, _Kinds) ->
    false.

%% Rewrite the arithmetic in one block of the function. Only the operation
%% and the literal operand change; the destination, the annotations, the
%% position and the terminators are untouched.
reduce_block(L, Kinds, Blocks0) ->
    Blk0 = maps:get(L, Blocks0),
    Is = [reduce_instr(I, Kinds) || I <- Blk0#b_blk.is],
    Blocks0#{L := Blk0#b_blk{is=Is}}.

%% Canonicalise the operand order of a commutative arithmetic instruction that
%% was written with a literal on the left (`C + V', `C * V') into the canonical
%% `V Op C' shape, so that every recurrence and every product has one syntactic
%% form. `+' and `*' are commutative for every number (a float sum/product does
%% not depend on the operand order) and fail on exactly the same non-number
%% operands, so no value and no exception changes. This is what lets the step
%% `1 + V' be recognised as the same iteration step as `V + 1'.
reduce_instr(#b_set{op={bif,Op}, args=[#b_literal{}=A, #b_var{}=B], anno=Anno}=I,
             Kinds)
  when Op =:= '+'; Op =:= '*' ->
    reduce_instr(I#b_set{args=[B, A], anno=swap_arg_types(Anno)}, Kinds);
reduce_instr(#b_set{op={bif,'*'}, args=[A, B]}=I, Kinds) ->
    case strength_mul(A, B, I, Kinds) of
        no -> I;
        {Op, Args} -> I#b_set{op={bif,Op}, args=Args}
    end;
reduce_instr(#b_set{op={bif,'div'}, args=[A, B]}=I, Kinds) ->
    case strength_div(A, B, I, Kinds) of
        no -> I;
        {Op, Args} -> I#b_set{op={bif,Op}, args=Args}
    end;
reduce_instr(#b_set{op={bif,'rem'}, args=[A, B]}=I, Kinds) ->
    case strength_rem(A, B, I, Kinds) of
        no -> I;
        {Op, Args} -> I#b_set{op={bif,Op}, args=Args}
    end;
reduce_instr(I, _Kinds) ->
    I.

%% `arg_types' is indexed by operand position, so canonicalising `C Op V' into
%% `V Op C' has to move the annotation of the variable with it. Only variable
%% operands are ever annotated (`beam_ssa_type:opt_anno_types/2' skips every
%% other operand), so only the entry of the variable operand can be present.
swap_arg_types(#{arg_types := AT}=Anno) ->
    case AT of
        #{0 := T0, 1 := T1} -> Anno#{arg_types => #{0 => T1, 1 => T0}};
        #{1 := T1} -> Anno#{arg_types => #{0 => T1}};
        #{0 := T0} -> Anno#{arg_types => #{1 => T0}};
        _ -> Anno
    end;
swap_arg_types(Anno) ->
    Anno.

strength_mul(#b_literal{val=K}, X, I, Kinds) when is_integer(K) ->
    strength_mul_1(K, X, operand_kind(I, 1, X, Kinds));
strength_mul(X, #b_literal{val=K}, I, Kinds) when is_integer(K) ->
    strength_mul_1(K, X, operand_kind(I, 0, X, Kinds));
strength_mul(_A, _B, _I, _Kinds) ->
    no.

strength_mul_1(K, X, Kind) when K > 1 ->
    case power_of_two(K) of
        {ok, N} ->
            case Kind of
                nat ->
                    {'bsl', [X, #b_literal{val=N}]};
                int ->
                    {'bsl', [X, #b_literal{val=N}]};
                unknown when K =:= 2 ->
                    %% X * 2 == X + X for every number: adding a float to
                    %% itself doubles it without rounding, exactly like
                    %% multiplying by 2, and both fail on the same values.
                    {'+', [X, X]};
                unknown ->
                    no
            end;
        no ->
            no
    end;
strength_mul_1(_K, _X, _Kind) ->
    %% A literal factor of 0 or 1 is not a shift and `X * 0'/'X * 1' still
    %% raise for a non-number, so those are left alone.
    no.

strength_div(X, #b_literal{val=K}, I, Kinds) when is_integer(K), K > 1 ->
    case power_of_two(K) of
        {ok, N} ->
            case operand_kind(I, 0, X, Kinds) of
                nat ->
                    %% `div' truncates toward zero, `bsr' floors; they agree
                    %% exactly for a non-negative dividend, and K >= 2 means
                    %% the original could not have been a division by zero.
                    {'bsr', [X, #b_literal{val=N}]};
                _ ->
                    no
            end;
        no ->
            no
    end;
strength_div(_X, _Y, _I, _Kinds) ->
    %% A divisor that is not a literal power of two >= 2 is left alone: a
    %% divisor of 0 must keep raising, 1 is not a shift, and the rest has no
    %% exact shift form.
    no.

%% `X rem 2^n' for a NON-NEGATIVE dividend is `X band (2^n - 1)': for a
%% dividend >= 0 `rem' and the bit mask agree on every value (`rem' truncates
%% toward zero and `band' takes the low bits, which is the same for a
%% non-negative number), both raise `badarith' on the same non-integer
%% operands, and K >= 2 means the original cannot have been a division by
%% zero. For a dividend that may be negative the two disagree, so the rewrite
%% is skipped.
strength_rem(X, #b_literal{val=K}, I, Kinds) when is_integer(K), K > 1 ->
    case power_of_two(K) of
        {ok, _N} ->
            case operand_kind(I, 0, X, Kinds) of
                nat -> {'band', [X, #b_literal{val=K - 1}]};
                _ -> no
            end;
        no ->
            no
    end;
strength_rem(_X, _Y, _I, _Kinds) ->
    no.
%% {ok, N} when K =:= 1 bsl N and K > 0.
power_of_two(K) when K > 0, K band (K - 1) =:= 0 ->
    {ok, pow2_shift(K, 0)};
power_of_two(_K) ->
    no.

pow2_shift(1, N) -> N;
pow2_shift(K, N) -> pow2_shift(K bsr 1, N + 1).
