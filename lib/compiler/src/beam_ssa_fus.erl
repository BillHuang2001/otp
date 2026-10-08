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
%%% a single, exactly equivalent, cheaper one. Three transformations are
%%% implemented; all are opt-in (compile option `fusion_opt') and share the
%%% rule that nothing is rewritten unless every condition checked by the pass
%%% holds, so every uncertain case is left untouched.
%%%
%%% Fusion 1 and Fusion 2 are "map lookup fusion": they remove map key *tests*
%%% whose result is already implied by a following map key *extraction*, so
%%% that the map is probed once instead of twice.
%%%
%%% Fusion 4 is "arithmetic constant-chain fusion": it folds a multiplication
%%% by an integer constant whose result is immediately multiplied by a second
%%% integer constant into a single multiplication by the product of the two
%%% constants.
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
%%% Fusion 4 (implemented): multiplication by a constant chain. When the last
%%% two instructions of a block are
%%%
%%%     D1 = bif:'*'(V, K1)                      (K1 an integer literal)
%%%     Bool = succeeded:body(D1)
%%%     br Bool, LSucc, LFail
%%%
%%% and LSucc consists of nothing but
%%%
%%%     D2 = bif:'*'(D1, K2)                     (K2 an integer literal)
%%%     B2 = succeeded:body(D2)
%%%     br B2, LFinal, LFail2
%%%
%%% and LSucc is reachable only from the multiplication block, then the two
%%% multiplications are replaced by a single one in the *first* block:
%%%
%%%     D2 = bif:'*'(V, K1 * K2)
%%%     Bool = succeeded:body(D2)
%%%     br Bool, LFinal, LFail
%%%
%%% LSucc is then unreachable and is dropped, and its definitions are gone:
%%% `D1' must not be used anywhere else, and the `D2' definition (which keeps
%%% its name and therefore all of its uses, including any beyond LSucc) moves
%%% to the first block, which dominates everything LSucc dominated because
%%% LSucc's only predecessor is the first block.
%%%
%%% Semantics preserved:
%%%
%%%   * `K1 * K2' is evaluated at compile time by the compiler itself; integer
%%%     arithmetic in Erlang is exact and unbounded (bignums), so it cannot
%%%     overflow and is associative. For any integer V,
%%%     `(V * K1) * K2 = V * (K1 * K2)' exactly.
%%%   * The rewrite is only applied when the type analysis has proven the
%%%     *first operand* (V) to be an integer (`#t_integer{}'), which makes
%%%     both constants integers too. This is essential: `*' is NOT associative
%%%     for floats (`(X * 0.1) * 0.1 =/= X * 0.01'), and an untyped operand
%%%     could be a float at run time.
%%%   * The failure set is unchanged. `V * K1' raises `badarith' exactly when V
%%%     is not a number, which is impossible for a proven integer, so neither
%%%     multiplication can raise and the fused one cannot either. The first
%%%     multiplication keeps its failure edge (LFail) untouched; the failure
%%%     edge LFail2 of the removed multiplication (which could only be taken if
%%%     `D1', a number, were not a number) simply becomes unreachable, exactly
%%%     as when the type analysis proves a guard cannot fail.
%%%   * Instruction order and the error position are unchanged: the fused
%%%     multiplication takes the position of the first one (keeping its source
%%%     location annotation), and no instruction is added to any block.
%%%   * The type annotations of the rewritten instruction are dropped, since
%%%     the value it now defines is different; they are recomputed by the type
%%%     passes on the next iteration.
%%%
%%% The gain is one multiplication and one guard test less per fused link; a
%%% chain of n constant multiplications collapses into a single one through
%%% repeated applications (each application drops one block).
%%%
%%% Nothing is rewritten unless all of the conditions checked by
%%% `maybe_fuse_4/8' hold; every uncertain case is left untouched. In
%%% particular, an earlier multiplication whose intermediate result is used
%%% anywhere else, or a chain link whose block is not exclusively reached from
%%% the multiplication, is left alone.
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
%% the function contains a block that is a candidate for one of the
%% transformations - either a block whose last instruction is a
%% `has_map_field' test on the same variable as the block's conditional branch,
%% and whose success edge either starts with a `get_map_element' of the same
%% map and key (Fusion 1) or is again a `has_map_field' test of the same map
%% with the same failure label (a possible Fusion 2 test chain), or a block
%% whose branch is on the success guard of a multiplication by an integer
%% constant and whose successor starts with a multiplication of its result by
%% another integer constant (Fusion 4).
%%
%% This is only a NECESSARY condition - a cheap filter that lets the driver
%% skip the pass for functions that cannot fuse anything. Whether the fusion
%% is actually legal (the map operand must be type-proven to be a map, the
%% extraction must be the only thing on the success edge, the two chains must
%% cover the same keys, ...) is decided by the pass itself.
-spec eligible({#opt_st{}, term()}) -> boolean().
eligible({#opt_st{ssa=SSA}, _FuncDb}) ->
    has_candidate(maps:from_list(normalize(SSA))).

has_candidate(Blocks) ->
    lists:any(fun({_L,Blk}) ->
                      candidate_block(Blk, Blocks)
                          orelse mul_candidate(Blk, Blocks)
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

%% Fusion 4 candidate: the block ends with a multiplication by an integer
%% constant (guarded by its `succeeded' test), and the successor starts with a
%% multiplication of that result by another integer constant.
-spec mul_candidate(beam_ssa:b_blk(), beam_ssa:block_map()) -> boolean().
mul_candidate(Blk, Blocks) ->
    case mul_head(Blk) of
        {_Prefix,_Guard,_Mul,D,_V,_K1,LSucc} ->
            case Blocks of
                #{LSucc := #b_blk{is=[#b_set{op={bif,'*'},
                                             args=[D,#b_literal{val=K2}]}|_]}}
                when is_integer(K2) -> true;
                _ -> false
            end;
        false ->
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
%% Fusion driver: Fusion 1 first, then Fusion 2, then Fusion 4.
%%============================================================================

-spec fuse_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_blk(LA, Blk, Blocks) ->
    case fuse_1_blk(LA, Blk, Blocks) of
        no ->
            case fuse_2_blk(LA, Blk, Blocks) of
                no -> fuse_4_blk(LA, Blk, Blocks);
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
%% Fusion 4: (V * K1) * K2 -> V * (K1 * K2), K1 and K2 integers
%%============================================================================

%% The multiplication chain shape at the end of a block:
%%
%%     ...                                              (Prefix)
%%     D = bif:'*'(V, K)                                (Mul)
%%     B = succeeded:D                                  (Guard)
%%     br B, LSucc, LFail
%%
%% where `K' is an integer literal and `V' is a variable. Returns the pieces of
%% that shape, or `false' for any other block. Shared by the eligibility
%% precondition (`mul_candidate/2') and by the Fusion 4 rewrite
%% (`fuse_4/10'), so that both look for exactly the same shape.
-spec mul_head(beam_ssa:b_blk()) ->
          false | {[beam_ssa:b_set()], beam_ssa:b_set(), beam_ssa:b_set(),
                   beam_ssa:b_var(), beam_ssa:b_var(), integer(),
                   beam_ssa:label()}.
mul_head(#b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc}}) ->
    case lists:reverse(Is) of
        [#b_set{op={succeeded,_},dst=Bool,args=[#b_var{}=D]}=Guard,
         #b_set{op={bif,'*'},dst=D,
                args=[#b_var{}=V,#b_literal{val=K}]}=Mul | RevPrefix]
        when is_integer(K) ->
            {lists:reverse(RevPrefix), Guard, Mul, D, V, K, LSucc};
        _ ->
            false
    end;
mul_head(_Blk) ->
    false.

%% Entry point for Fusion 4 at a block whose branch is on the success guard of
%% a multiplication by an integer constant.
-spec fuse_4_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_4_blk(LA, #b_blk{last=#b_br{}=Br}=Blk, Blocks) ->
    case mul_head(Blk) of
        {Prefix, Guard, Mul, D1, V, K1, LSucc} ->
            fuse_4(LA, Prefix, Guard, Mul, D1, V, K1, Br, LSucc, Blocks);
        false ->
            no
    end;
fuse_4_blk(_LA, _Blk, _Blocks) ->
    no.

fuse_4(LA, Prefix, Guard, Mul, D1, V, K1, Br, LSucc, Blocks) ->
    case Blocks of
        #{LSucc := #b_blk{is=[#b_set{op={bif,'*'},dst=D2,
                                     args=[D1,#b_literal{val=K2}]},
                                #b_set{op={succeeded,_},dst=B2,args=[D2]}],
                          last=#b_br{bool=B2,succ=LFinal,fail=LFail2}}}
        when is_integer(K2) ->
            Type = anno_arg_type(Mul),
            case maybe_fuse_4(LA, LSucc, LFinal, LFail2, D1, B2, Type, Blocks) of
                true ->
                    %% The type annotations of the rewritten instruction are
                    %% dropped: the value it now defines is a different one.
                    Anno = maps:without([arg_types,result_type], Mul#b_set.anno),
                    Mul1 = Mul#b_set{anno=Anno, dst=D2,
                                     args=[V,#b_literal{val=K1 * K2}]},
                    Is = Prefix ++ [Mul1, Guard#b_set{args=[D2]}],
                    A1 = #b_blk{is=Is, last=Br#b_br{succ=LFinal}},
                    {yes,maps:remove(LSucc, Blocks#{LA => A1})};
                false ->
                    no
            end;
        #{} ->
            no
    end.

%% All the conditions that must hold for the Fusion 4 rewrite to be provably
%% valid.
-spec maybe_fuse_4(beam_ssa:label(), beam_ssa:label(), beam_ssa:label(),
                   beam_ssa:label(), beam_ssa:b_var(), beam_ssa:b_var(),
                   false | term(), beam_ssa:block_map()) -> boolean().
maybe_fuse_4(LA, LSucc, LFinal, LFail2, D1, B2, Type, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    %% `*' is exact for integers but not associative for floats, so the operand
    %% must be proven to be an integer. This is what makes the two constants
    %% integers and `K1 * K2' a valid compile-time fold.
    is_integer_type(Type)
    %% LSucc is removed and the definition of D2 moves to LA. LA must therefore
    %% be LSucc's only predecessor, so that it dominates everything LSucc
    %% dominated; D2 (with an unchanged value) can then keep all of its uses.
    andalso preds(LSucc, Blocks) =:= [LA]
    %% The new branch of LA must not reference the removed block or itself.
    andalso LSucc =/= LFinal
    andalso LFinal =/= LA
    %% LSucc is replaced and LFinal is branched to; the exception pseudo block
    %% is not an ordinary block and must not be part of the rewrite. LFail is
    %% not checked: it is kept unchanged as the failure edge of the first
    %% multiplication, and is the exception block for a guard failure.
    andalso lists:all(fun(L) -> L =/= Special end, [LA,LSucc,LFinal])
    %% D1 is the intermediate result, which the rewrite eliminates. It may only
    %% be used by the multiplication that consumes it and by the guard of that
    %% multiplication; any other use would observe the eliminated value.
    andalso use_count(D1, Blocks) =:= 2
    %% The guard of the removed multiplication is used only by its branch.
    andalso use_count(B2, Blocks) =:= 1
    %% Phi nodes would have to be rewritten when the predecessor changes; in
    %% particular LFail2 becomes unreachable, so its phi would keep an entry for
    %% a block that no longer exists. It is cheaper (and safer) to simply not
    %% fuse in that case.
    andalso not has_phi(LSucc, Blocks)
    andalso not has_phi(LFinal, Blocks)
    andalso not has_phi(LFail2, Blocks).%% The type of the first argument of the instruction, as recorded by
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

%% True when the type analysis has proven the value to be an integer. Only
%% integers may be folded: `*' is exact and associative for them, while it is
%% neither for floats.
-spec is_integer_type(false | term()) -> boolean().
is_integer_type(#t_integer{}) -> true;
is_integer_type(_) -> false.
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
