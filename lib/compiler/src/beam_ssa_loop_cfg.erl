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
-module(beam_ssa_loop_cfg).
-moduledoc false.

%%% Control-flow helpers shared by the opt-in loop passes
%%% (`beam_ssa_loop_opt', `beam_ssa_loop_arith' and `beam_ssa_loop_reduce').
%%%
%%% This module owns the CFG vocabulary those passes are written in, and
%%% nothing else:
%%%
%%% * the successor/predecessor relation (`succs/1', `preds/2') and the
%%%   dominator relation (`dominators/2', an iterative computation over the
%%%   reverse-postorder block list).
%%% * natural-loop recognition (`find_loops/2'): an edge H->P is a back edge
%%%   when H dominates P, and the body of the loop is {H} plus every block
%%%   that can reach P without passing through H. Loops sharing a header are
%%%   merged.
%%% * the iteration region of a loop (`loop_region/3'): the natural loop body
%%%   plus every block the header dominates.
%%% * the definition set of a region (`loop_defs/2').
%%% * the preheader of a loop header (`ensure_preheader/4'), which inserts a
%%%   dedicated block on the preheader->header edge when the predecessor has
%%%   other successors and redirects the header's phi arguments accordingly.
%%% * a cheap O(V+E) "does this function have any back edge" test
%%%   (`has_back_edge/1'), used as the eligibility gate of the loop pass, which
%%%   deliberately avoids computing dominators.
%%%
%%% The block list handed in and out is always in reverse postorder (RPO). In
%%% RPO every edge of acyclic control flow goes from an earlier to a later
%%% block, while every cycle produces at least one edge whose target is not
%%% after its source, which is what makes `has_back_edge/1' a sound
%%% over-approximation of "this function has a loop".
-export([preds/2, succs/1, succs_last/1, dominators/2, find_loops/2,
         order_linear/2, has_back_edge/1, loop_region/3, loop_defs/2,
         ensure_preheader/4]).

-include("beam_ssa.hrl").

%%% ------------------------------------------------------------------
%%% Successors and predecessors
%%% ------------------------------------------------------------------

-spec succs(beam_ssa:b_blk()) -> [beam_ssa:label()].
succs(#b_blk{last=Last}) ->
    succs_last(Last).

-spec succs_last(beam_ssa:terminator()) -> [beam_ssa:label()].
succs_last(#b_br{succ=S, fail=F}) when S =:= F -> [S];
succs_last(#b_br{succ=S, fail=F}) -> [S, F];
succs_last(#b_switch{fail=F, list=Ls}) ->
    lists:usort([F | [L || {_, L} <- Ls]]);
succs_last(#b_ret{}) ->
    [].

-spec preds([beam_ssa:label()], beam_ssa:block_map()) ->
          beam_ssa:predecessor_map().
preds(Labels, Blocks) ->
    lists:foldl(
      fun(L, Acc) ->
              Ss = succs(maps:get(L, Blocks)),
              lists:foldl(fun(S, A) ->
                                  maps:update_with(
                                    S, fun(Ls) -> [L | Ls] end, [L], A)
                          end, Acc, Ss)
      end, #{L => [] || L <- Labels}, Labels).

%%% ------------------------------------------------------------------
%%% Dominators
%%% ------------------------------------------------------------------

%% Iterative dominator computation. Returns #{Label => [Dominator]} where the
%% list includes the label itself.
-spec dominators([beam_ssa:label()], beam_ssa:block_map()) ->
          beam_ssa:dominator_map().
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
%%% Loop recognition
%%% ------------------------------------------------------------------

-spec find_loops([beam_ssa:label()], beam_ssa:block_map()) ->
          {[{beam_ssa:label(), [beam_ssa:label()]}], beam_ssa:dominator_map()}.
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
-spec loop_region(beam_ssa:label(), [beam_ssa:label()],
                  beam_ssa:dominator_map()) -> [beam_ssa:label()].
loop_region(H, Order0, Doms) ->
    [L || L <- Order0, lists:member(H, maps:get(L, Doms, []))].

-spec loop_defs([beam_ssa:label()], beam_ssa:block_map()) ->
          ordsets:ordset(beam_ssa:b_var()).
loop_defs(Body, Blocks) ->
    lists:foldl(
      fun(L, Acc) ->
              Is = (maps:get(L, Blocks))#b_blk.is,
              lists:foldl(fun(#b_set{dst=V}, A) ->
                                  ordsets:add_element(V, A)
                          end, Acc, Is)
      end, ordsets:new(), Body).

%%% ------------------------------------------------------------------
%%% Preheader
%%% ------------------------------------------------------------------

%% Make sure `Pre' is a dedicated preheader of `H'. If `Pre' has no other
%% successor than `H' it already is one. Otherwise insert a fresh block on the
%% Pre->H edge and redirect the header's phi arguments coming from `Pre'.
-spec ensure_preheader(beam_ssa:label(), beam_ssa:label(), beam_ssa:block_map(),
                       beam_ssa:label()) ->
          {beam_ssa:block_map(), beam_ssa:label(), beam_ssa:label()}.
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
%%% Ordering and the cheap back-edge test
%%% ------------------------------------------------------------------

%% Preserve the original (RPO) block order and append freshly created
%% preheader blocks at the end.
-spec order_linear([beam_ssa:label()], beam_ssa:block_map()) ->
          [{beam_ssa:label(), beam_ssa:b_blk()}].
order_linear(Order0, Blocks) ->
    New = lists:sort([L || L <- maps:keys(Blocks),
                           not lists:member(L, Order0)]),
    [{L, maps:get(L, Blocks)} || L <- Order0 ++ New].

%% Whether the function has any edge that is a back edge in RPO. In reverse
%% postorder every edge of acyclic control flow goes from an earlier to a
%% later block, while every cycle produces at least one edge whose target is
%% not after its source. Testing "some successor is not later than the block
%% itself" is therefore a sound over-approximation of "this function has a
%% loop", and it costs O(V+E) with no dominator computation.
-spec has_back_edge([{beam_ssa:label(), beam_ssa:b_blk()}]) -> boolean().
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
