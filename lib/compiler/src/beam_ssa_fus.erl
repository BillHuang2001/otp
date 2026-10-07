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

%%%
%%% This pass performs "map lookup fusion": it rewrites a map key *test*
%%% followed by a map key *extraction* of the same key into a single
%%% extraction. That is, the map is looked up once instead of twice.
%%%
%%% Fusion 1 (implemented): when the last instruction of a block is
%%%
%%%     has_map_field(Map, Key) -> Bool
%%%     br Bool, LSucc, LFail
%%%
%%% and the block LSucc (= the success edge) consists of nothing but
%%%
%%%     get_map_element(Map, Key) -> GV
%%%     succeeded:guard GV -> Bool2
%%%     br Bool2, LSS, LFail2
%%%
%%% then the test is redundant: `has_map_field` returning true implies both
%%% that Map is a map and that Key is present, so the `get_map_element` on the
%%% success edge can never fail. The pair is therefore replaced by a single
%%% `get_map_element` in the *first* block, with the failure edge redirected to
%%% LFail (the test's "false" label) instead of LFail2:
%%%
%%%     get_map_element(Map, Key) -> GV
%%%     succeeded:guard GV -> Bool2
%%%     br Bool2, LSS, LFail
%%%
%%% The second block is then unreachable and is dropped. The map is thus looked
%%% up once (`get_map_elements` in the generated BEAM code) instead of twice
%%% (`has_map_fields`/`is_map_key` plus `get_map_elements`).
%%%
%%% Semantics preserved:
%%%
%%%   * The *failure* set of the fused lookup is exactly the input set for
%%%     which the original test was false (Key missing from a map). A failing
%%%     `get_map_elements` writes nothing to its destination, so GV is never
%%%     bound on the LFail path, just as before. A key that is present takes
%%%     the same LSS edge as before, with the same value in GV.
%%%   * The asymmetry between `is_map_key/2` (which is false for a missing key)
%%%     and `maps:get/2` (which raises `{badkey,Key}`) is handled by routing the
%%%     fused lookup's failure to the *test's* false label, never to the
%%%     extraction's own failure label. LFail2 becomes unreachable, which is
%%%     sound because a successful `has_map_field` already guarantees that the
%%%     extraction cannot fail.
%%%   * `get_map_elements` requires a map operand; the pass therefore only runs
%%%     when the type analysis has proven that Map is a map at the test site.
%%%     This is what keeps `has_map_field(non_map, Key) -> false` (a branch)
%%%     from turning into an unsafe map operation.
%%%   * The source location of the extraction (`anno`) is retained, and the
%%%     test's instruction (which cannot raise) is dropped, so error positions
%%%     and stack traces are unchanged. Instruction order is unchanged: the
%%%     fused lookup occupies exactly the position of the old test.
%%%   * The type annotations of the moved instruction are dropped so that no
%%%     stale type information leaks into the result; they are recomputed by
%%%     the type passes on the next iteration.
%%%
%%% Nothing is rewritten unless all of the conditions checked by `maybe_fuse/9`
%%% hold; every uncertain case is left untouched.
%%%
%%% Fusion 2 (deliberately NOT implemented here): merging a chain of single-key
%%% `get_map_element/2` operations on the same map and with the same failure
%%% label into one multi-key lookup. This cannot be expressed in BEAM SSA:
%%% `get_map_elements/3` exists only as a *BEAM* instruction (genop.tab:158),
%%% not as an SSA op - `beam_ssa:op()` has no such member and
%%% `beam_ssa_codegen:cg_block/3` matches `get_map_element` with exactly the
%%% two arguments `[Map,Key]`. Emitting a multi-key form would require a new
%%% IR operation plus matching support in the code generator, the validator,
%%% the liveness/register machinery and the alias analysis. Note also that the
%%% BEAM-level merge is already performed: `beam_block:opt_maps/2` coalesces
%%% adjacent `get_map_elements` instructions into one multi-pair
%%% `get_map_elements` instruction, which is exactly what a chain of
%%% single-successor `get_map_element` blocks lowers to. Implementing an SSA
%%% version today would therefore add risk without adding value.
%%%
%%% Fusion 3 (deliberately NOT implemented here): `get_tuple_element/3` chains.
%%% `ssa_opt_element/1` (prologue passes) plus `ssa_opt_get_tuple_element/1`
%%% (late epilogue passes) already reorder and coalesce tuple element
%%% extractions, and `genop.tab` has no fused multi-element instruction (only
%%% `66: get_tuple_element/3`), so there is no instruction for the pass to
%%% emit. Out of scope.
%%%

-module(beam_ssa_fus).
-moduledoc false.

-export([ssa_opt_fus/1, eligible/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").

-import_record(beam_ssa_opt, [opt_st, func_info]).

%%============================================================================
%% Driver
%%============================================================================

-spec ssa_opt_fus({#opt_st{}, beam_ssa_opt:func_info_db()}) ->
          {#opt_st{}, beam_ssa_opt:func_info_db()}.
ssa_opt_fus({#opt_st{ssa=Blocks0}=St, FuncDb}) ->
    Blocks = normalize(Blocks0),
    {St#opt_st{ssa=fuse(Blocks)}, FuncDb}.

%% Eligibility precondition used by the opt-in per-pass budget/watchdog
%% driver (`beam_ssa_opt_budget', wired in `beam_ssa_opt:gate/1'): true when
%% the function contains a block that ends with a `has_map_field' test whose
%% success edge starts with a `get_map_element' of the same key.
%%
%% This is only a NECESSARY condition - a cheap filter that lets the driver
%% skip the pass for functions that cannot fuse anything. Whether the fusion
%% is actually legal (the map operand must be type-proven to be a map) is
%% decided by the pass itself.
-spec eligible({#opt_st{}, term()}) -> boolean().
eligible({#opt_st{ssa=SSA}, _FuncDb}) ->
    has_candidate(maps:from_list(normalize(SSA))).

has_candidate(Blocks) ->
    lists:any(fun({_L,Blk}) -> candidate_block(Blk, Blocks) end,
              maps:to_list(Blocks)).

candidate_block(#b_blk{is=Is,last=#b_br{succ=LSucc}}, Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,args=[Map,Key]}, _|_] ->
            candidate_succ(Blocks, LSucc, Map, Key);
        _ ->
            false
    end;
candidate_block(_Blk, _Blocks) ->
    false.

candidate_succ(Blocks, LSucc, Map, Key) ->
    case Blocks of
        #{LSucc := #b_blk{is=[#b_set{op=get_map_element,args=[Map,Key]}|_]}} ->
            true;
        _ ->
            false
    end.

%% The pass runs inside the repeated (fixpoint) phase, where the SSA is kept as
%% a linearized list. Be defensive in case the representation has been changed
%% by some other pass: always return a list.
-spec normalize(beam_ssa:block_map() | [{beam_ssa:label(),beam_ssa:b_blk()}]) ->
          [{beam_ssa:label(),beam_ssa:b_blk()}].
normalize(Blocks) when is_list(Blocks) ->
    Blocks;
normalize(Blocks) when is_map(Blocks) ->
    beam_ssa:linearize_only(Blocks).

-spec fuse([{beam_ssa:label(),beam_ssa:b_blk()}]) ->
          [{beam_ssa:label(),beam_ssa:b_blk()}].
fuse(Linear0) ->
    Blocks0 = maps:from_list(Linear0),
    case scan(Linear0, Blocks0) of
        false ->
            Linear0;
        {true,Blocks} ->
            %% A block was removed and the control flow rewired. Restart the
            %% scan from the updated blocks so that we never inspect a stale
            %% block. Each rewrite drops one block, so this terminates.
            fuse(beam_ssa:linearize_only(Blocks))
    end.

-spec scan([{beam_ssa:label(),beam_ssa:b_blk()}], beam_ssa:block_map()) ->
          false | {true,beam_ssa:block_map()}.
scan([{L,Blk} | Bs], Blocks) ->
    case fuse_blk(L, Blk, Blocks) of
        no -> scan(Bs, Blocks);
        {yes,Blocks1} -> {true,Blocks1}
    end;
scan([], _Blocks) ->
    false.

%%============================================================================
%% Fusion 1: has_map_field + get_map_element -> get_map_element
%%============================================================================

-spec fuse_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_blk(LA, #b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc,fail=LFail}=BrA},
         Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,dst=Bool,args=[Map,Key]}=Hmf | RevPrefix] ->
            fuse_1(LA, lists:reverse(RevPrefix), BrA, Hmf,
                   Map, Key, LSucc, LFail, Blocks);
        _ ->
            no
    end;
fuse_blk(_LA, _Blk, _Blocks) ->
    no.

fuse_1(LA, Prefix, BrA, Hmf, Map, Key, LSucc, LFail, Blocks) ->
    case Blocks of
        #{LSucc := #b_blk{is=[#b_set{op=get_map_element,
                                     dst=GV,args=[Map,Key]}=Get,
                                #b_set{op={succeeded,_},
                                       dst=GetBool,args=[GV]}=Succeeded],
                          last=#b_br{bool=GetBool,succ=LSS,fail=LFail2}}} ->
            HmfBool = Hmf#b_set.dst,
            HmfType = anno_arg_type(Hmf),
            case maybe_fuse(LA, LSucc, LSS, LFail, LFail2,
                            HmfBool, HmfType, GetBool, Blocks) of
                true ->
                    %% Drop the type annotations of the moved instruction; the
                    %% types that were valid at the extraction site need not be
                    %% valid at the test site.
                    Anno = maps:remove(arg_types, Get#b_set.anno),
                    Get1 = Get#b_set{anno=Anno},
                    Br = BrA#b_br{bool=GetBool, succ=LSS, fail=LFail},
                    A1 = #b_blk{is=Prefix ++ [Get1,Succeeded], last=Br},
                    Blocks1 = maps:remove(LSucc, Blocks#{LA => A1}),
                    {yes,Blocks1};
                false ->
                    no
            end;
        #{} ->
            no
    end.

%% All the conditions that must hold for the rewrite to be provably valid.
-spec maybe_fuse(beam_ssa:label(), beam_ssa:label(), beam_ssa:label(),
                 beam_ssa:label(), beam_ssa:label(), beam_ssa:b_var(),
                 false | term(), beam_ssa:b_var(), beam_ssa:block_map()) ->
          boolean().
maybe_fuse(LA, LSucc, LSS, LFail, LFail2, HmfBool, HmfType, GetBool, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    %% The exception pseudo block is special and must not be part of the
    %% rewrite (a branch to it means "raise", and its fail edge cannot be
    %% re-targeted).
    lists:all(fun(L) -> L =/= Special end, [LA,LSucc,LSS,LFail,LFail2])
    andalso LSucc =/= LSS                       %LSucc's br would become a self loop
    andalso LSS =/= LA                          %LSS must not be the rewritten block
    andalso LFail =/= LSucc                     %A would reference a removed block
    %% LSucc is removed, so it must have exactly one predecessor (LA); that
    %% makes all its former instructions unreachable and keeps the moved
    %% definitions dominated by LA.
    andalso preds(LSucc, Blocks) =:= [LA]
    %% Redirecting the failure edge of LSucc to LFail must not create a new
    %% entry into LSS, and must make LFail2 unreachable, so nothing below them
    %% can observe the changed set of definitions.
    andalso preds(LSS, Blocks) =:= [LSucc]
    andalso preds(LFail2, Blocks) =:= [LSucc]
    %% Phi nodes would have to be rewritten when the predecessors change; it
    %% is cheaper (and safer) to simply not fuse in that case.
    andalso not has_phi(LSucc, Blocks)
    andalso not has_phi(LSS, Blocks)
    andalso not has_phi(LFail, Blocks)
    andalso not has_phi(LFail2, Blocks)
    %% The test result is consumed only by the branch we rewrite, and the
    %% extraction's success bool only by the branch we drop.
    andalso use_count(GetBool, Blocks) =:= 1
    andalso use_count(HmfBool, Blocks) =:= 1
    %% `get_map_elements` requires a map operand, while `has_map_field` maps a
    %% non-map to "false". Only fuse when Map is provably a map here.
    andalso is_map_type(HmfType).

%% The type of the first argument of the instruction, as recorded by
%% beam_ssa_type. `false` when unknown.
-spec anno_arg_type(beam_ssa:b_set()) -> false | term().
anno_arg_type(#b_set{anno=Anno}) ->
    case Anno of
        #{arg_types := #{0 := Type}} -> Type;
        #{} -> false
    end.

-spec is_map_type(false | term()) -> boolean().
is_map_type(#t_map{}) -> true;
is_map_type(_) -> false.

-spec preds(beam_ssa:label(), beam_ssa:block_map()) -> [beam_ssa:label()].
preds(L, Blocks) ->
    [From || {From, #b_blk{last=Last}} <- maps:to_list(Blocks),
             lists:member(L, successors(Last))].

-spec successors(beam_ssa:terminator()) -> [beam_ssa:label()].
successors(#b_br{succ=S,fail=F}) ->
    [S,F];
successors(#b_switch{fail=F,list=Ls}) ->
    [F | [T || {_,T} <- Ls]];
successors(#b_ret{}) ->
    [].

-spec has_phi(beam_ssa:label(), beam_ssa:block_map()) -> boolean().
has_phi(L, Blocks) ->
    case Blocks of
        #{L := #b_blk{is=Is}} -> lists:any(fun is_phi/1, Is);
        #{} -> false
    end.

-spec is_phi(beam_ssa:b_set()) -> boolean().
is_phi(#b_set{op=phi}) -> true;
is_phi(_) -> false.

-spec use_count(beam_ssa:b_var(), beam_ssa:block_map()) -> non_neg_integer().
use_count(Var, Blocks) ->
    maps:fold(fun(_L, #b_blk{is=Is,last=Last}, Acc) ->
                      Acc + count_is(Var, Is) + count_last(Var, Last)
              end, 0, Blocks).

-spec count_is(beam_ssa:b_var(), [beam_ssa:b_set()]) -> non_neg_integer().
count_is(V, [#b_set{op=phi,args=Args}|Is]) ->
    phi_count(V, Args) + count_is(V, Is);
count_is(V, [#b_set{args=Args}|Is]) ->
    arg_count(V, Args) + count_is(V, Is);
count_is(_V, []) ->
    0.

-spec phi_count(beam_ssa:b_var(), [beam_ssa:argument()]) ->
          non_neg_integer().
phi_count(V, [{V0,_}|T]) when V0 =:= V -> 1 + phi_count(V, T);
phi_count(V, [_|T]) -> phi_count(V, T);
phi_count(_V, []) -> 0.

-spec arg_count(beam_ssa:b_var(), [beam_ssa:argument()]) ->
          non_neg_integer().
arg_count(V, [A|T]) when A =:= V -> 1 + arg_count(V, T);
arg_count(V, [_|T]) -> arg_count(V, T);
arg_count(_V, []) -> 0.

-spec count_last(beam_ssa:b_var(), beam_ssa:terminator()) ->
          non_neg_integer().
count_last(V, #b_br{bool=B}) -> eq(V, B);
count_last(V, #b_switch{arg=A}) -> eq(V, A);
count_last(V, #b_ret{arg=A}) -> eq(V, A);
count_last(_V, _Last) -> 0.

-spec eq(term(), term()) -> 0 | 1.
eq(A, B) -> case A =:= B of true -> 1; false -> 0 end.
