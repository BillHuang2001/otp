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
%%% This pass performs "operation fusion": it replaces a pair of operations by
%%% a single, exactly equivalent, cheaper one. Five transformations are
%%% implemented; all are opt-in (compile option `fusion_opt') and share the
%%% rule that nothing is rewritten unless every condition checked by the pass
%%% holds, so every uncertain case is left untouched.
%%%
%%% Fusion 1 and Fusion 2 are "map lookup fusion": they remove map key *tests*
%%% whose result is already implied by a following map key *extraction*, so
%%% that the map is probed once instead of twice. Fusion 5 is the same
%%% redundancy for the `maps:get/2' spelling of an extraction.
%%%
%%% Fusion 4 and Fusion 6 are "arithmetic constant-chain fusion": they fold a
%%% chain of two operations by an integer constant into a single operation by a
%%% constant folded at compile time - a multiplication chain (Fusion 4) and the
%%% division/remainder chains (Fusion 6). Those two, together with the small SSA
%%% vocabulary the whole pass is written in (predecessors, phi detection, use
%%% counting, argument types) and the reasoning that makes a fold exact and
%%% exception preserving, live in the sibling module `beam_ssa_fus_arith'; this
%%% module delegates to it exactly as it does to its own map fusions.
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
%%% Fusion 2 (implemented) generalises Fusion 1 to a whole chain of map key
%%% tests that is redundant because the extractions cover exactly the tested
%%% keys - see the comment above `fuse_2_blk/3' below.
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
%%% Fusion 2 (implemented): the same redundancy, generalised to *several*
%%% keys. When the map operand is type-proven to be a map and a chain of
%%% single-successor blocks
%%%
%%%     LA:  ... has_map_field(Map, K1) -> T1 ; br T1, LB, LFail
%%%     LB:  ... has_map_field(Map, K2) -> T2 ; br T2, LC, LFail
%%%     ...
%%%     LZ:  ... has_map_field(Map, Kn) -> Tn ; br Tn, LSucc, LFail     (n >= 2)
%%%
%%% is followed by a chain of single-successor extraction blocks
%%%
%%%     LSucc: get_map_element(Map, K1') -> G1 ; succeeded:guard G1 ; br -> .., LFail2
%%%     ...
%%%     LEnd:  get_map_element(Map, Kn') -> Gn ; succeeded:guard Gn ; br -> LFinal, LFail2
%%%
%%% whose keys are exactly the tested keys (as a set) and which share one
%%% failure label LFail2, then the whole test chain is redundant and is
%%% replaced by the extraction chain: every tested key is looked up again (and
%%% can therefore only succeed) by the extraction chain. Each `has_map_field'
%%% test block is rewritten into the corresponding extraction (matched by key),
%%% and the extraction chain's own blocks are dropped. Exactly as in Fusion 1,
%%% the resulting lookups fail to LFail (the tests' "false" label), which is
%%% sound because the extraction set already covers the tested keys, so the
%%% extraction chain's own failure label LFail2 becomes unreachable. The map is
%%% thus probed once for the extractions instead of once for the tests *plus*
%%% once for the extractions.
%%%
%%% Expressibility: this needs NO new IR operation nor new BEAM instruction.
%%% The pass only rewrites each test block in place into an equally shaped
%%% single-key extraction (`get_map_element/2` + `succeeded:guard`, which
%%% `beam_ssa_codegen:cg_block/3` already matches as the two-element form
%%% `[get_map_element, succeeded]`), keeping one lookup per block; the
%%% multi-pair `get_map_elements/3` BEAM instruction is then synthesised by
%%% `beam_block:opt_maps/2`, which coalesces the adjacent same-failure
%%% `get_map_elements` instructions the chain lowers to. (Merging the
%%% extraction chain into a single SSA block would *not* be expressible: there
%%% is no multi-key SSA op, and `cg_block/3` has no clause for several
%%% `[get_map_element, succeeded]' pairs in one block. The pass therefore keeps
%%% the block structure and leaves the coalescing to `beam_block`.)
%%%
%%% Fusion 3 (deliberately NOT implemented here): `get_tuple_element/3` chains.
%%% `ssa_opt_element/1` (prologue passes) plus `ssa_opt_get_tuple_element/1`
%%% (late epilogue passes) already reorder and coalesce tuple element
%%% extractions, and `genop.tab` has no fused multi-element instruction (only
%%% `66: get_tuple_element/3`), so there is no instruction for the pass to
%%% emit. Out of scope.
%%%
%%% Fusion 4 (implemented) and Fusion 6 (implemented) are the arithmetic
%%% constant chains, in the sibling module `beam_ssa_fus_arith`, whose
%%% moduledoc describes the shape they match, the folds, the safety argument of
%%% each of them and the arithmetic chains that are deliberately not folded:
%%%
%%%   * Fusion 4: `(V * K1) * K2' -> `V * (K1 * K2)'.
%%%   * Fusion 6 (a): `(V div K1) div K2' -> `V div (K1 * K2)'.
%%%   * Fusion 6 (b): `(V rem K1) rem K2' -> `V rem K2' (K1 a multiple of K2).
%%%   * Fusion 6 (c): `(V * K1) div K2' -> `V * (K1 div K2)' (K2 divides K1,
%%%     and the intermediate `V * K1' is provably bounded).
%%%
%%% All of them require the operand to be type-proven to be an integer, and
%%% nothing is rewritten unless all of the conditions checked by
%%% `beam_ssa_fus_arith' hold; every uncertain case is left untouched.
%%%
%%% Fusion 5 (implemented) is Fusion 1's redundancy for the `maps:get/2'
%%% spelling of a map extraction. `maps:get/2' is a guard BIF and is therefore
%%% compiled to the `map_get/2' instruction (never to `get_map_element'), which
%%% the front end pairs with a `{succeeded,body}' marker:
%%%
%%%     has_map_field(Map, Key) -> Bool ; br Bool, LSucc, LFail
%%%     LSucc: D = bif:map_get(Key, Map)
%%%            SBool = succeeded:body D
%%%            br SBool, LSS, LFail2
%%%
%%% is replaced by
%%%
%%%     D = get_map_element(Map, Key)
%%%     SBool = succeeded:guard D
%%%     br SBool, LSS, LFail
%%%
%%% `D' and `SBool' keep their names, so every use (and any use beyond LSucc)
%%% stays valid; only the instruction that computes D changes, and LSucc is
%%% dropped. The map is again probed once instead of twice, and the reasoning
%%% is identical to Fusion 1's (`has_map_field' true implies Map is a map and
%%% Key is present, so `map_get' cannot raise). See the comment above
%%% `fuse_5_blk/3'. `LFail2' is not required to be absent here (it normally
%%% *is* the exception pseudo block, which `map_get' branches to on failure),
%%% only to stay consistent when it keeps other predecessors.
%%%
%%% Other "chains" that are deliberately NOT fused here:
%%%
%%%   * Map updates: `beam_ssa_opt:ssa_opt_cse' already merges adjacent
%%%     associative `put_map' chains (`combine_put_maps/2'), and the *exact*
%%%     variant must not be merged even though it is not: for
%%%     `M#{a := 1}#{b := 2}' the sequential form raises the missing key of the
%%%     *first* update, while a single multi-key `put_map_exact' raises the
%%%     missing key of the *last*, so the exception would change. Records:
%%%     `ssa_opt_merge_updates/1' already merges adjacent `update_record' and
%%%     `put_tuple'+`update_record' chains.
%%%   * Constant `+`/`-` chains: `beam_ssa_type:reassociate/3' already
%%%     reassociates them (and folds `+0'/`-0'/`*1'/`*0').
%%%   * List construction: a chain of `put_list/2' building a fixed list has no
%%%     cheaper equivalent - there is no single "list literal" run-time
%%%     instruction (literal lists are built through the literal pool by the
%%%     loader), and the elements of the list are not known to be constants.
%%%     Out of scope.
%%%   * Arithmetic chains whose constants do not divide (`X * C1 div C2' with
%%%     C2 not dividing C1 is NOT exact, since integer division truncates:
%%%     `5 * 3 div 2' is 7 while `5 * (3 div 2)' is 5), a chain through a zero
%%%     divisor (`X div 0'/`X rem 0' raise `badarith'), an operand that is not
%%%     type-proven to be an integer (a float or untyped operand may be a
%%%     float at run time, and `*' is not associative for floats), an operand
%%%     whose magnitude cannot be proven bounded where the fold removes a
%%%     growing intermediate product, and a chain whose first operand is the
%%%     constant rather than the variable: all declined, with the full
%%%     reasoning, in `beam_ssa_fus_arith'. `(X * C1) rem C2' is expressible but
%%%     saves no instruction, and nested `bsl'/`bsr' chains are folded by
%%%     `sys_core_fold' (or, for unbounded shift counts, must not be rewritten
%%%     at all, since `erts_bsl' raises `system_limit').
%%%   * `get_map_element' chains: `D1 = map_get(K1, M), D2 = map_get(K2, D1)'
%%%     and friends. There is no fused BEAM instruction for a map lookup whose
%%%     operand is the result of another map lookup, and `map_get/2' on a value
%%%     that is not a map raises `{badmap,Term}' - so nothing can be folded
%%%     into a single instruction. Out of scope.
%%%
%%% Fusion 3 (`get_tuple_element/3' chains) and the partial-overlap case of
%%% Fusion 2 are discussed next to Fusion 1/Fusion 2 above.
%%%
-module(beam_ssa_fus).
-moduledoc false.

-export([ssa_opt_fus/1, eligible/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").

%% The SSA vocabulary and the arithmetic constant-chain fusions (Fusion 4 and
%% Fusion 6) live in the sibling module `beam_ssa_fus_arith'.
-import(beam_ssa_fus_arith,
        [preds/2, has_phi/2, use_count/2, anno_arg_type/1, is_map_type/1]).

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
%% the function contains a block that is a candidate for one of the
%% transformations - either a block whose last instruction is a
%% `has_map_field' test on the same variable as the block's conditional branch,
%% and whose success edge either starts with a `get_map_element' of the same
%% map and key (Fusion 1) or with the `map_get/2' lookup of the same map and
%% key (Fusion 5), or is again a `has_map_field' test of the same map
%% with the same failure label (a possible Fusion 2 test chain), or the head of
%% an arithmetic constant chain (Fusion 4/6, whose own precondition is
%% `beam_ssa_fus_arith:candidate/2').
%%
%% This is only a NECESSARY condition - a cheap filter that lets the driver
%% skip the pass for functions that cannot fuse anything. Whether the fusion
%% is actually legal (the map operand must be type-proven to be a map, the
%% extraction must be the only thing on the success edge, the two chains must
%% cover the same keys, the operand of an arithmetic chain must be type-proven
%% to be an integer, ...) is decided by the pass itself.
-spec eligible({#opt_st{}, term()}) -> boolean().
eligible({#opt_st{ssa=SSA}, _FuncDb}) ->
    has_candidate(maps:from_list(normalize(SSA))).

has_candidate(Blocks) ->
    lists:any(fun({_L,Blk}) ->
                      candidate_block(Blk, Blocks)
                          orelse beam_ssa_fus_arith:candidate(Blk, Blocks)
              end, maps:to_list(Blocks)).
candidate_block(#b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc,fail=LFail}}, Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,dst=Bool,args=[Map,Key]}|_] ->
            candidate_succ(Blocks, LSucc, Map, Key, LFail);
        _ ->
            false
    end;
candidate_block(_Blk, _Blocks) ->
    false.
candidate_succ(Blocks, LSucc, Map, Key, LFail) ->
    case Blocks of
        #{LSucc := #b_blk{is=[#b_set{op=get_map_element,args=[Map,Key]}|_]}} ->
            %% Fusion 1 candidate: the success edge starts with the extraction
            %% of the tested key.
            true;
        #{LSucc := #b_blk{is=[#b_set{op={bif,map_get},args=[Key,Map]}|_]}} ->
            %% Fusion 5 candidate: the success edge starts with the `map_get/2'
            %% lookup of the tested key (note the reversed argument order).
            true;
        #{LSucc := #b_blk{is=Is,last=#b_br{bool=B,fail=LFail}}} ->
            %% Fusion 2 candidate: the success edge starts another map-key test
            %% on the same map with the same failure label (so the test chain
            %% has at least two elements).
            case lists:reverse(Is) of
                [#b_set{op=has_map_field,dst=B,args=[Map,_]}|_] -> true;
                _ -> false
            end;
        _ ->
            false
    end.

%% The pass runs inside the repeated (fixpoint) phase, where the SSA is kept as%% a linearized list. Be defensive in case the representation has been changed
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
%% Fusion driver: Fusion 1, then Fusion 2, then the arithmetic constant chains
%% (Fusion 4 and Fusion 6, implemented in `beam_ssa_fus_arith'), then Fusion 5.
%%============================================================================

-spec fuse_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_blk(LA, Blk, Blocks) ->
    case fuse_1_blk(LA, Blk, Blocks) of
        no ->
            case fuse_2_blk(LA, Blk, Blocks) of
                no ->
                    case beam_ssa_fus_arith:fuse_blk(LA, Blk, Blocks) of
                        no -> fuse_5_blk(LA, Blk, Blocks);
                        {yes,_}=Yes -> Yes
                    end;
                {yes,_}=Yes -> Yes
            end;
        {yes,_}=Yes -> Yes
    end.
%%============================================================================
%% Fusion 1: has_map_field + get_map_element -> get_map_element
%%============================================================================

-spec fuse_1_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_1_blk(LA, #b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc,fail=LFail}=BrA},
           Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,dst=Bool,args=[Map,Key]}=Hmf | RevPrefix] ->
            fuse_1(LA, lists:reverse(RevPrefix), BrA, Hmf,
                   Map, Key, LSucc, LFail, Blocks);
        _ ->
            no
    end;
fuse_1_blk(_LA, _Blk, _Blocks) ->
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

%%============================================================================
%% Fusion 2: chain of map-key tests -> chain of map-key extractions
%%============================================================================

%% Entry point for Fusion 2 at a block LA whose last instruction is a
%% `has_map_field' test on the branch variable. LA must be the *first* block of
%% the test chain (a chain of length 1 is Fusion 1's job, so Fusion 2 requires
%% at least two tests).
-spec fuse_2_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_2_blk(LA, #b_blk{is=Is,last=#b_br{bool=T,succ=Succ,fail=LFail}=Br},
           Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,dst=T}=Hmf | RevPrefix] ->
            Entry = {LA, lists:reverse(RevPrefix), Br, Hmf},
            case test_chain(Blocks, Succ, map0(Hmf), LFail, [Entry]) of
                {Tests, LSucc} when length(Tests) >= 2 ->
                    fuse_2(LFail, Tests, LSucc, Blocks);
                _ ->
                    no
            end;
        _ ->
            no
    end;
fuse_2_blk(_LA, _Blk, _Blocks) ->
    no.

%% The map operand of a `has_map_field' instruction.
-spec map0(beam_ssa:b_set()) -> beam_ssa:argument().
map0(#b_set{args=[Map,_]}) -> Map.

%% Collect the maximal chain of map-key tests starting at `L' (a successor of
%% the block at the front of `Acc'). Every block of the chain must end with
%% `has_map_field(Map, _)' on the branch variable and branch to the next test
%% block or, for the last one, to the extraction chain, with the same failure
%% label LFail throughout.
-spec test_chain(beam_ssa:block_map(), beam_ssa:label(), beam_ssa:argument(),
                 beam_ssa:label(), [tuple()]) -> {[tuple()], beam_ssa:label()}.
test_chain(Blocks, L, Map, LFail, Acc) ->
    case Blocks of
        #{L := #b_blk{is=Is,last=#b_br{bool=T,succ=Succ,fail=LFail}=Br}} ->
            case lists:reverse(Is) of
                [#b_set{op=has_map_field,dst=T,args=[Map,_]}=Hmf | RevPrefix] ->
                    Entry = {L, lists:reverse(RevPrefix), Br, Hmf},
                    case is_test_block(Blocks, Succ, Map, LFail) of
                        true -> test_chain(Blocks, Succ, Map, LFail, [Entry|Acc]);
                        false -> {lists:reverse([Entry|Acc]), Succ}
                    end;
                _ ->
                    {lists:reverse(Acc), L}
            end;
        _ ->
            {lists:reverse(Acc), L}
    end.

%% True when `L' is again a map-key test block on the same map, with the same
%% failure label.
-spec is_test_block(beam_ssa:block_map(), beam_ssa:label(), beam_ssa:argument(),
                    beam_ssa:label()) -> boolean().
is_test_block(Blocks, L, Map, LFail) ->
    case Blocks of
        #{L := #b_blk{is=Is,last=#b_br{bool=T,fail=LFail}}} ->
            case lists:reverse(Is) of
                [#b_set{op=has_map_field,dst=T,args=[Map,_]}|_] -> true;
                _ -> false
            end;
        _ ->
            false
    end.

-spec fuse_2(beam_ssa:label(), [tuple()], beam_ssa:label(),
             beam_ssa:block_map()) -> no | {yes, beam_ssa:block_map()}.
fuse_2(LFail, Tests, LSucc, Blocks) ->
    {_L, _Prefix, _Br, Hmf} = hd(Tests),
    case is_map_type(anno_arg_type(Hmf)) of
        false ->
            no;
        true ->
            case extract_chain(Blocks, LSucc, map0(Hmf)) of
                {Extracts, LFinal, LFail2} ->
                    maybe_fuse_2(LFail, Tests, LSucc, Extracts, LFinal,
                                 LFail2, Blocks);
                no ->
                    no
            end
    end.

%% Collect the chain of single-key extractions starting at `LSucc'. Every block
%% must consist of nothing but `get_map_element(Map, _)' + `succeeded:guard'
%% and branch to the next extraction block (or, for the last one, to LFinal)
%% with the same failure label LFail2.
-spec extract_chain(beam_ssa:block_map(), beam_ssa:label(),
                    beam_ssa:argument()) ->
          no | {[tuple()], beam_ssa:label(), beam_ssa:label()}.
extract_chain(Blocks, L, Map) ->
    case Blocks of
        #{L := #b_blk{is=[#b_set{op=get_map_element,dst=G,args=[Map,Key]}=Get,
                               #b_set{op={succeeded,_},dst=T,args=[G]}=Succeeded],
                      last=#b_br{bool=T,succ=Succ,fail=LFail2}}} ->
            Entry = {L, Key, G, T, Get, Succeeded},
            case is_extract_block(Blocks, Succ, Map, LFail2) of
                true ->
                    case extract_chain(Blocks, Succ, Map) of
                        {Es, LFinal, LFail2} -> {[Entry|Es], LFinal, LFail2};
                        no -> no
                    end;
                false ->
                    {[Entry], Succ, LFail2}
            end;
        _ ->
            no
    end.

-spec is_extract_block(beam_ssa:block_map(), beam_ssa:label(),
                       beam_ssa:argument(), beam_ssa:label()) -> boolean().
is_extract_block(Blocks, L, Map, LFail2) ->
    case Blocks of
        #{L := #b_blk{is=[#b_set{op=get_map_element,args=[Map,_]},
                           #b_set{op={succeeded,_},dst=T,args=[_]}],
                      last=#b_br{bool=T,fail=LFail2}}} ->
            true;
        _ ->
            false
    end.

%% All the conditions that must hold for the Fusion 2 rewrite to be provably
%% valid.
-spec maybe_fuse_2(beam_ssa:label(), [tuple()], beam_ssa:label(), [tuple()],
                   beam_ssa:label(), beam_ssa:label(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
maybe_fuse_2(LFail, Tests, LSucc, Extracts, LFinal, LFail2, Blocks) ->
    case fusion_2_ok(Tests, LSucc, Extracts, LFinal, LFail, LFail2, Blocks) of
        true -> {yes,rewrite_2(Tests, Extracts, LFinal, LFail, Blocks)};
        false -> no
    end.

-spec fusion_2_ok([tuple()], beam_ssa:label(), [tuple()], beam_ssa:label(),
                  beam_ssa:label(), beam_ssa:label(), beam_ssa:block_map()) ->
          boolean().
fusion_2_ok(Tests, LSucc, Extracts, LFinal, LFail, LFail2, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    TestBlks = [L || {L,_,_,_} <- Tests],
    Removed = [L || {L,_,_,_,_,_} <- Extracts],
    TestKeys = [K || {_,_,_,#b_set{args=[_,K]}} <- Tests],
    ExKeys = [K || {_,K,_,_,_,_} <- Extracts],
    LastTest = lists:last(TestBlks),
    %% The two chains must cover exactly the same set of keys, and the keys on
    %% each side must be distinct so that the test->extraction match is a
    %% bijection.
    length(TestKeys) =:= length(ExKeys)
    andalso lists:sort(TestKeys) =:= lists:sort(ExKeys)
    andalso lists:usort(TestKeys) =:= lists:sort(TestKeys)
    andalso lists:usort(ExKeys) =:= lists:sort(ExKeys)
    %% The exception pseudo block is special and must not take part in the
    %% rewrite.
    andalso lists:all(fun(L) -> L =/= Special end,
                      [LFail, LFail2, LFinal | Removed])
    %% The removed chain must not contain the labels we keep referring to.
    andalso not lists:member(LFinal, TestBlks)
    andalso not lists:member(LFinal, Removed)
    andalso not lists:member(LFail, TestBlks)
    andalso not lists:member(LFail, Removed)
    andalso not lists:member(LFail2, Removed)
    andalso LFinal =/= LastTest
    %% Every removed extraction block must have exactly one predecessor (the
    %% previous block of the chain, or the last test block for the first one).
    andalso preds(LSucc, Blocks) =:= [LastTest]
    andalso single_preds(Removed, Blocks)
    %% The predecessor of LFinal changes (and chain blocks disappear), so phi
    %% nodes would have to be rewritten; do not fuse in that case.
    andalso not has_phi(LFinal, Blocks)
    andalso not has_phi(LFail, Blocks)
    andalso not has_phi(LFail2, Blocks)
    andalso lists:all(fun({L,_,_,_,_,_}) -> not has_phi(L, Blocks) end, Extracts)
    %% Each test result is consumed only by the branch we rewrite.
    andalso lists:all(fun({_,_,_,#b_set{dst=T}}) ->
                              use_count(T, Blocks) =:= 1
                      end, Tests).

%% True when every element of the list (in order) has its predecessor being the
%% element before it in the list.
-spec single_preds([beam_ssa:label()], beam_ssa:block_map()) -> boolean().
single_preds([_A, B | T], Blocks) ->
    preds(B, Blocks) =:= [_A] andalso single_preds([B|T], Blocks);
single_preds(_, _Blocks) ->
    true.

%% Replace each test block by the matching extraction (matched by key) and drop
%% the original extraction chain.
-spec rewrite_2([tuple()], [tuple()], beam_ssa:label(), beam_ssa:label(),
                beam_ssa:block_map()) -> beam_ssa:block_map().
rewrite_2(Tests, Extracts, LFinal, LFail, Blocks) ->
    KeyMap = maps:from_list([{K,{Get,Succeeded}} ||
                                {_L,K,_G,_T,Get,Succeeded} <- Extracts]),
    LastTest = element(1, lists:last(Tests)),
    Blocks1 =
        lists:foldl(fun({L, Prefix, Br, #b_set{args=[_,Key]}}, Acc) ->
                            {Get0, Succeeded} = maps:get(Key, KeyMap),
                            %% Drop the type annotations of the moved
                            %% instruction; the types valid at the extraction
                            %% site need not be valid at the test site.
                            Anno = maps:remove(arg_types, Get0#b_set.anno),
                            Get = Get0#b_set{anno=Anno},
                            T = Succeeded#b_set.dst,
                            Succ = case L =:= LastTest of
                                       true -> LFinal;
                                       false -> Br#b_br.succ
                                   end,
                            Br1 = Br#b_br{bool=T, succ=Succ, fail=LFail},
                            Blk = #b_blk{is=Prefix ++ [Get, Succeeded], last=Br1},
                            Acc#{L => Blk}
                    end, Blocks, Tests),
    lists:foldl(fun(L, Acc) -> maps:remove(L, Acc) end, Blocks1, extract_labels(Extracts)).

-spec extract_labels([tuple()]) -> [beam_ssa:label()].
extract_labels(Extracts) -> [L || {L,_,_,_,_,_} <- Extracts].

%%============================================================================
%% The arithmetic constant chains (Fusion 4 and Fusion 6) and the SSA
%% vocabulary this pass is written in live in `beam_ssa_fus_arith'.
%%============================================================================

%%============================================================================
%% Fusion 5: has_map_field + map_get -> get_map_element
%%============================================================================
%% The same redundancy as Fusion 1, but for the other spelling of a map
%% extraction. `maps:get/2' is compiled to the `map_get/2' BIF (it is a guard
%% BIF, so it may not expand to a test instruction), which the front end pairs
%% with a `{succeeded,body}' marker:
%%
%%     LA:  ... has_map_field(Map, Key) -> Bool ; br Bool, LSucc, LFail
%%     LSucc: D = bif:map_get(Key, Map)
%%            SBool = succeeded:body D
%%            br SBool, LSS, LFail2
%%
%% The test makes the lookup redundant in exactly the same way: `has_map_field'
%% returning true implies both that Map is a map and that Key is present, so
%% `map_get' can never raise `{badkey,Key}' (nor `{badmap,Term}'). The pair is
%% therefore replaced by a single `get_map_element' in the *first* block, with
%% the failure edge redirected to LFail:
%%
%%     LA:  ... D = get_map_element(Map, Key)
%%              SBool = succeeded:guard D
%%              br SBool, LSS, LFail
%%
%% `D' keeps its name, so all of its uses stay valid: its uses were dominated
%% by LSucc and are still dominated by LA, which is LSucc's only predecessor.
%% `SBool' keeps its name too and is still consumed only by the branch. LSucc
%% is then unreachable and is dropped.
%%
%% The gain is the same as Fusion 1's: the map is probed once (`get_map_elements'
%% in the generated BEAM code) instead of once for the test plus once for the
%% lookup. It only fires when the type analysis has proven Map to be a map at
%% the test site, which is also what turns the `is_map_key/2' BIF into the
%% `has_map_field' instruction in the first place; without that proof (e.g. for
%% a map operand of unknown type) nothing is rewritten. The extraction is only
%% rewritten when the lookup is the *only* thing on the success edge of the
%% test, so the two blocks can be collapsed.
%%
%% Semantics preserved (identical to Fusion 1):
%%
%%   * The failure set is unchanged. A failing `get_map_elements' writes nothing
%%     to its destination, so D is never bound on the LFail path, exactly as it
%%     was never bound there before (the `map_get' lived in LSucc, on the
%%     success edge of the test). A present key takes the same LSS edge with the
%%     same value in D.
%%   * The `{badkey,Key}'/`{badmap,Term}' exception that `map_get/2' raises for
%%     a missing key or a non-map can no longer be raised - but it could not be
%%     raised before either, because the test that guards the lookup has already
%%     proven Map to be a map and Key to be present. LFail2 (the exception block
%%     that `map_get' branched to on failure) simply loses an edge, exactly as
%%     when the type analysis proves a guard cannot fail.
%%   * Instruction order and the error position are unchanged: the fused lookup
%%     takes the position of the test (keeping the lookup's source location, so
%%     a stack trace would still point at the `maps:get/2' call), and no
%%     instruction is added to any block.
%%   * The `succeeded' marker is emitted as a `guard' check, which is the
%%     canonical pairing for `get_map_element'
%%     (`beam_core_to_ssa:select_extract_map/4') and is what
%%     `beam_ssa_codegen:cg_block/3' matches.
%%
%% Nothing is rewritten unless all of the conditions checked by
%% `maybe_fuse_5/9' hold; every uncertain case is left untouched.
%%============================================================================

%% Entry point for Fusion 5 at a block LA whose last instruction is a
%% `has_map_field' test on the branch variable.
-spec fuse_5_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_5_blk(LA, #b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc,fail=LFail}=BrA},
           Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op=has_map_field,dst=Bool,args=[Map,Key]}=Hmf | RevPrefix] ->
            fuse_5(LA, lists:reverse(RevPrefix), BrA, Hmf, Map, Key,
                   LSucc, LFail, Blocks);
        _ ->
            no
    end;
fuse_5_blk(_LA, _Blk, _Blocks) ->
    no.

fuse_5(LA, Prefix, BrA, Hmf, Map, Key, LSucc, LFail, Blocks) ->
    case Blocks of
        #{LSucc := #b_blk{is=[#b_set{op={bif,map_get},
                                     dst=D,args=[Key,Map]}=GetMap,
                                #b_set{op={succeeded,_},
                                       dst=SBool,args=[D]}=Succeeded],
                          last=#b_br{bool=SBool,succ=LSS,fail=LFail2}}} ->
            HmfType = anno_arg_type(Hmf),
            case maybe_fuse_5(LA, LSucc, LSS, LFail, LFail2, Hmf#b_set.dst,
                              SBool, HmfType, Blocks) of
                true ->
                    %% The lookup's argument order is reversed (`map_get/2'
                    %% takes the key first), so its type annotations describe
                    %% the wrong arguments and must be dropped; the type passes
                    %% recompute them on the next iteration.
                    Anno = maps:without([arg_types,result_type],
                                        GetMap#b_set.anno),
                    Get = #b_set{op=get_map_element, dst=D, args=[Map,Key],
                                 anno=Anno},
                    Br = BrA#b_br{bool=SBool, succ=LSS, fail=LFail},
                    A1 = #b_blk{is=Prefix ++ [Get,Succeeded], last=Br},
                    {yes,maps:remove(LSucc, Blocks#{LA => A1})};
                false ->
                    no
            end;
        #{} ->
            no
    end.

%% All the conditions that must hold for the Fusion 5 rewrite to be provably
%% valid.
-spec maybe_fuse_5(beam_ssa:label(), beam_ssa:label(), beam_ssa:label(),
                   beam_ssa:label(), beam_ssa:label(), beam_ssa:b_var(),
                   beam_ssa:b_var(), false | term(), beam_ssa:block_map()) ->
          boolean().
maybe_fuse_5(LA, LSucc, LSS, LFail, LFail2, Bool, SBool, HmfType, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    %% The exception pseudo block is special and must not be part of the
    %% rewrite (a branch to it means "raise"). LFail2, on the other hand, *is*
    %% normally the exception block here, because `map_get' raises on failure;
    %% it is therefore only required to stay consistent, not to be absent - see
    %% the phi/LFail2 condition below.
    lists:all(fun(L) -> L =/= Special end, [LA,LSucc,LSS,LFail])
    andalso LSucc =/= LSS                       %LSucc's br would become a self loop
    andalso LSS =/= LA                          %LSS must not be the rewritten block
    andalso LFail =/= LSucc                     %A would reference a removed block
    %% LSucc is removed, so it must have exactly one predecessor (LA); that
    %% makes all its former instructions unreachable and keeps the moved
    %% definition of D dominated by LA.
    andalso preds(LSucc, Blocks) =:= [LA]
    %% Redirecting the success edge of the test to LSS must not create a new
    %% entry into LSS, so that LSS stays dominated by LA (and D stays dominated
    %% by its new definition site).
    andalso preds(LSS, Blocks) =:= [LSucc]
    %% The edge LSucc -> LFail2 disappears. If LFail2 then becomes unreachable
    %% that is fine (it can only have been reached through the lookup, which
    %% cannot fail); otherwise LFail2 must not have a phi node, whose entry for
    %% LSucc would become stale. LFail2 is *not* required to be unreachable:
    %% for `maps:get/2' it is the exception pseudo block, which is shared by
    %% every raising instruction in the function.
    andalso (preds(LFail2, Blocks) =:= [LSucc] orelse not has_phi(LFail2, Blocks))
    %% Phi nodes would have to be rewritten when the predecessors change; it
    %% is cheaper (and safer) to simply not fuse in that case.
    andalso not has_phi(LSucc, Blocks)
    andalso not has_phi(LSS, Blocks)
    andalso not has_phi(LFail, Blocks)
    %% The test result is consumed only by the branch we rewrite, and the
    %% lookup's success bool only by the branch we drop.
    andalso use_count(Bool, Blocks) =:= 1
    andalso use_count(SBool, Blocks) =:= 1
    %% `get_map_elements' requires a map operand, while `has_map_field' maps a
    %% non-map to "false" (and `map_get' would raise `{badmap,Term}' instead).
    %% Only fuse when Map is provably a map here.
    andalso is_map_type(HmfType).