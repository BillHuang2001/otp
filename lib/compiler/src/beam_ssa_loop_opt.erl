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
%%% This pass is opt-in; it only runs when the `loop_opt' compiler option is
%%% given (it is appended to the prologue pass list by `beam_ssa_opt' when the
%%% option is present). The pass can be turned off individually with
%%% `no_ssa_opt_loop_opt' (auto-derived by `beam_ssa_opt:passes_1/2') or
%%% together with all SSA optimizations with `no_ssa_opt'.
%%%
%%% WHAT IT DOES
%%%
%%% 1. Loop recognition. Natural loops are found from the function CFG: an
%%%    edge H->P is a back edge when H dominates P (including a self edge
%%%    H->H). The body of the natural loop of that edge is {H} plus every
%%%    block that reaches P without passing through H. Loops that share a
%%%    header are merged.
%%%
%%% 2. Loop-invariant code motion (LICM). A pure instruction whose operands
%%%    are all defined outside the loop (or are themselves already hoisted)
%%%    is moved into the loop preheader, i.e. the unique predecessor of the
%%%    loop header that lies outside the loop. Execution is therefore moved
%%%    from "once per iteration" to "once per loop entry".
%%%
%%% CORRECTNESS RULES (conservative by design -- when in doubt, do nothing)
%%%
%%% * Only `#b_set{}' value instructions are considered; phis, terminators,
%%%   calls and side-effecting instructions are never touched.
%%% * Only instructions that are in ?SAFE_OPS are hoisted. ?SAFE_OPS is a
%%%   whitelist of operations that are side-effect free AND can never raise
%%%   for any operand values (constructors and type tests). This is stricter
%%%   than `beam_ssa:no_side_effect/1', which is only a "can be omitted if
%%%   the result is unused" predicate and returns true for instructions that
%%%   can still fail (get_hd, get_tuple_element, ...). Hoisting an
%%%   instruction that may raise would execute it on paths where it was not
%%%   executed before, so those are deliberately excluded.
%%% * Consequently arithmetic is NOT hoisted here: without type information
%%%   (this pass runs before the SSA type pass) an operation such as `+'
%%%   cannot be proven non-raising. This is a deliberate, documented limit.
%%% * A preheader must exist or be creatable: the loop header must have
%%%   exactly one predecessor outside the loop. When that predecessor has
%%%   other successors as well, a fresh preheader block is inserted on the
%%%   edge (rewriting the phi arguments of the header accordingly).
%%%   If the header is the function entry block there is no outside
%%%   predecessor and the loop is left alone.
%%% * The whole transformation is wrapped so that any unexpected condition
%%%   simply skips the optimization rather than failing the compilation.
%%%
%%% NOT IMPLEMENTED (see the report / CONTEXT.md)
%%%
%%% * Unrolling. A sound unroller needs an exact trip count. The pass runs in
%%%   the prologue phase, i.e. before the SSA type analysis, so neither
%%%   `#t_integer{elements=...}' nor `beam_bounds' information is available;
%%%   a trip count could only be derived from a syntactic comparison against
%%%   literals, which is not enough to make unrolling worthwhile (or safe) in
%%%   the common case. Unrolling is therefore deferred rather than guessed.

-export([opt/1]).

-include("beam_ssa.hrl").
-import_record(beam_ssa_opt, [opt_st]).

%% Maximum number of instructions hoisted per loop. Bounds the growth of the
%% preheader on pathological input.
-define(MAX_HOIST, 64).

%% Operations that are side-effect free and that can never raise, for any
%% operand values. Everything not listed here is left in the loop.
-define(SAFE_OPS, [put_list, put_tuple, is_nonempty_list, is_tagged_tuple]).

-spec opt({#opt_st{}, term()}) -> {#opt_st{}, term()}.
opt({#opt_st{ssa=Linear0}=St, FuncDb}) when is_list(Linear0) ->
    %% Any unexpected condition skips the optimization; it must never turn a
    %% compilable module into a compilation error.
    Linear = try
                 opt_linear(Linear0)
             catch
                 _:_ -> Linear0
             end,
    {St#opt_st{ssa=Linear}, FuncDb};
opt({St, FuncDb}) ->
    {St, FuncDb}.

%%% ------------------------------------------------------------------
%%% Driver
%%% ------------------------------------------------------------------

opt_linear(Linear0) ->
    Blocks0 = maps:from_list(Linear0),
    Order0 = [L || {L, _} <- Linear0],
    case Order0 of
        [] ->
            Linear0;
        [_ | _] ->
            Loops = find_loops(Order0, Blocks0),
            case Loops of
                [] ->
                    Linear0;
                _ ->
                    Next = lists:max([0 | Order0]) + 1,
                    {Blocks, _} =
                        lists:foldl(fun(Loop, {B, N}) ->
                                            opt_loop(Loop, Order0, B, N)
                                    end, {Blocks0, Next}, Loops),
                    order_linear(Order0, Blocks)
            end
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
    [{H, ordsets:to_list(Body)} || {H, Body} <- Loops].

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
%%% Per-loop transformation
%%% ------------------------------------------------------------------

opt_loop({H, Body}, Order0, Blocks0, NextLabel) ->
    BodySet = ordsets:from_list(Body),
    Preds = preds(maps:keys(Blocks0), Blocks0),
    Outside = [P || P <- maps:get(H, Preds, []),
                    not ordsets:is_element(P, BodySet)],
    case Outside of
        [Pre] when Pre =/= H, is_map_key(Pre, Blocks0) ->
            {Blocks1, Pre1, Next1} = ensure_preheader(Pre, H, Blocks0, NextLabel),
            Blocks2 = licm(H, Body, Order0, Pre1, Blocks1),
            {Blocks2, Next1};
        _ ->
            {Blocks0, NextLabel}
    end.

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
    lists:member(Op, ?SAFE_OPS) andalso
        lists:all(fun(A) -> invariant_arg(A, Inv) end, Args).

invariant_arg(#b_literal{}, _Inv) ->
    true;
invariant_arg(#b_var{}=V, Inv) ->
    not ordsets:is_element(V, Inv);
invariant_arg(_, _) ->
    false.
