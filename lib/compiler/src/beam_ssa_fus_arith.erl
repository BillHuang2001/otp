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
%%% Arithmetic constant-chain fusion: the arithmetic half of the opt-in
%%% `fusion_opt' pass, whose driver is `beam_ssa_fus'. That module delegates to
%%% `fuse_blk/3' here exactly as it delegates to its own map fusions, and asks
%%% `candidate/2' from its eligibility precondition.
%%%
%%% Four transformations are implemented - Fusion 4 and the three Fusion 6
%%% folds (a), (b) and (c) below. They all share the rule of the pass: nothing
%%% is rewritten unless every condition checked here holds, so every uncertain
%%% case is left untouched, and nothing is rewritten at all unless the
%%% `fusion_opt' option is present (the driver is only appended to the repeated
%%% pass list then).
%%%
%%% This module also owns the small SSA vocabulary the whole fusion pass is
%%% written in (`preds/2', `has_phi/2', `use_count/2', `anno_arg_type/1',
%%% `is_map_type/1'): the map fusions of `beam_ssa_fus' use exactly the same
%%% vocabulary, and keeping a single copy keeps the two halves of the pass from
%%% disagreeing about what "used nowhere else" means.
%%%
%%% THE SHAPES
%%%
%%% The two operations of a chain are matched in one of two block shapes: the
%%% two-block shape - in three tail variants, depending on what the type
%%% analysis has already proven about the two operations - and the same-block
%%% shape. All of them are folded by exactly the same table (`fold/5'), the same
%%% construction (`fused_op/5') and the same rewrite rule: the fused instruction
%%% takes the position (and the source location) of the first operation and the
%%% NAME of the second one, so every use of `D2' stays valid and the
%%% intermediate `D1' disappears.
%%%
%%% (1) The two-block shape. The first block LA ends with
%%%
%%%     ...                                          (Prefix)
%%%     D1 = bif:Op1(V, K1)                          (K1 an integer literal)
%%%     Bool = succeeded:body D1                     (Guard)
%%%     br Bool, LSucc, LFail
%%%
%%% and LSucc - whose only predecessor must be LA - starts with the second
%%% operation of the chain, applied to the result of the first one:
%%%
%%%     D2 = bif:Op2(D1, K2)                         (K2 an integer literal)
%%%
%%% `(Op1, Op2)' must be one of the pairs listed by `fold/5'. The fold is then
%%% a single instruction `bif:FusedOp(V, FusedK)', which is put *in place of
%%% the first operation*, keeping the first operation's position and source
%%% location. `D2' keeps its name, so every use of `D2' - including uses beyond
%%% LSucc - stays valid, and the intermediate `D1' disappears. In every
%%% implemented fold the first operation is the one that survives, so the fused
%%% BIF is always the BIF of the first operation; in the two-block shape this is
%%% what lets the rewrite reuse the first operation's `{succeeded,_}' guard
%%% unchanged.
%%%
%%% The three supported tail shapes differ in what the type analysis has already
%%% done to the two operations.
%%%
%%% Guarded (the shape of Fusion 4; `Guard' is reused for the fused
%%% instruction, and the guard's destination is LA's branch variable):
%%%
%%%     LSucc: D2 = bif:Op2(D1, K2)
%%%            B2 = succeeded:body D2
%%%            br B2, LFinal, LFail2
%%%
%%%     LA:    ... D2 = bif:FusedOp(V, FusedK)
%%%                Bool = succeeded:body D2         (the same Guard instr.)
%%%                br Bool, LFinal, LFail
%%%
%%%     LSucc is unreachable and is dropped; the failure edge of the removed
%%%     guard, LFail2, becomes unreachable.
%%%
%%% Unguarded (the type analysis has already proven that `Op2' cannot fail, so
%%% its `{succeeded,_}' marker and the branch on it are gone):
%%%
%%%     LSucc: D2 = bif:Op2(D1, K2)                  (then anything)
%%%            <any terminator>
%%%
%%%     LA:    ... D2 = bif:FusedOp(V, FusedK)
%%%                Bool = succeeded:body D2         (the same Guard instr.)
%%%                br Bool, LSucc, LFail            (unchanged)
%%%     LSucc: <the rest of its instructions>        (Op2 removed)
%%%            <the same terminator>
%%%
%%%     The block structure is kept here: LA still ends on LSucc, and LSucc -
%%%     left with at most an empty instruction list and a branch - keeps its
%%%     predecessors, its successors and their phi nodes untouched. A later
%%%     pass (`ssa_opt_merge_blocks' in the late epilogue) folds the emptied
%%%     block away, so nothing is left behind in the generated code.
%%%
%%% Guard-free (the type analysis has proven that the FIRST operation cannot
%%% fail either, so its `{succeeded,_}' marker and the branch on it are gone as
%%% well; both edges of LA's branch now lead to LSucc):
%%%
%%%     LA:    ... D1 = bif:Op1(V, K1)
%%%            br LSucc, LSucc                      (unconditional)
%%%
%%%     LA:    ... D2 = bif:FusedOp(V, FusedK)
%%%            br LSucc, LSucc                      (unchanged)
%%%     LSucc: <the rest of its instructions>        (Op2 removed)
%%%            <the same terminator>
%%%
%%%     Again the block structure is kept - and this shape is not the same-block
%%%     one below: the guard of the removed operation
%%%     (`ssa_opt_live'/the type analysis) is not the same thing as the *merge*
%%%     of its blocks, which only `ssa_opt_merge_blocks' performs in the late
%%%     epilogue, i.e. after this pass has run. A chain whose first operation
%%%     has been proven non-raising therefore sits in two blocks joined by an
%%%     unconditional branch while this pass sees it, and it is this shape -
%%%     not the same-block one - that Fusion 6 (c) and the bounded `div'/`rem'
%%%     chains are matched in (their non-raising proof is exactly the bound
%%%     `fold/5' requires, see `bounded_operand/1'): without it, the only shape
%%%     left in the compiler for them would be one that does not exist yet.
%%%
%%% The three tail shapes are recognized by `match/2' (through
%%% `match_two_block/2' and `match_no_guard/2'), and `fuse/3' decides which one
%%% applies.
%%%
%%% (2) The same-block shape. If the two blocks of a chain have already been
%%% merged - or were never split, e.g. inside code built by a different route -
%%% the two operations are adjacent instructions of one block:
%%%
%%%     ...                                          (Prefix)
%%%     D1 = bif:Op1(V, K1)                          (K1 an integer literal)
%%%     D2 = bif:Op2(D1, K2)                         (K2 an integer literal)
%%%     ...                                          (Rest)
%%%
%%% The pair is folded in place when the two instructions are *adjacent* - so
%%% putting the fused instruction where the first operation was cannot reorder
%%% anything - and when `D1' is used exactly once in the whole function
%%% (`use_count/2'), that single use being the second operation. Any other use,
%%% in this block's own tail, in another block or in a phi node, would observe
%%% the eliminated value. The pair is then replaced inside the same block by the
%%% single instruction
%%%
%%%     D2 = bif:FusedOp(V, FusedK)
%%%
%%% which leaves the block's terminator, the CFG (predecessors, successors and
%%% their phi nodes) and every other instruction exactly as they were: nothing
%%% is moved across a block boundary, so no dominance or phi question can arise.
%%%
%%% This shape is the one to fall back on when a chain has been merged into one
%%% block before this pass runs. It is deliberately the LAST one `match/2' looks
%%% for: a chain that is still split into two blocks is described by one of the
%%% two-block shapes above, and those are the shapes the compiler actually
%%% produces for a chain that straddles a guard (the guard, and with it the
%%% block split, is created by `beam_core_to_ssa' for every operation that may
%%% fail, and only the *guard* - not the split - is undone by the type analysis).
%%%
%%% The operand types are read from the first operation's `arg_types'
%%% annotation in every shape (`anno_arg_type/1'). `match/2' tries the two
%%% two-block shapes first and falls back to the same-block one, and `fuse/3'
%%% folds them through `fuse_guarded/5', `fuse_unguarded/5', `fuse_no_guard/5'
%%% and `fuse_same_block/5' respectively.
%%%
%%% WHAT IS FOLDED
%%%
%%% Fusion 4 (the multiplication chain), `(V * K1) * K2' -> `V * (K1 * K2)':
%%% exact because integer arithmetic is exact and unbounded, so it is
%%% associative and `K1 * K2' is folded at compile time. The gain is one
%%% multiplication and one guard test less per fused link; a chain of n
%%% constant multiplications collapses into one through repeated applications
%%% (each application drops one block).
%%%
%%% Fusion 6 (a), the division chain: `(V div K1) div K2' -> `V div (K1 * K2)'
%%% for integer literals K1, K2 with `K1 * K2 =/= 0'. Exact because integer
%%% division truncates toward zero and truncation composes:
%%% `trunc(trunc(x/k1)/k2) = trunc(x/(k1*k2))' for every nonzero k1, k2 (for
%%% any sign of the divisors, since `trunc(-y) = -trunc(y)'). Exception
%%% preserving because neither the rewritten nor the fused instruction can
%%% raise: `div' never raises on integers with a nonzero divisor, and the
%%% intermediate `V div K1' is never larger in magnitude than V, so the fused
%%% divisor `K1*K2' cannot introduce a size that the original chain did not
%%% already produce.
%%%
%%% Fusion 6 (b), the remainder chain: `(V rem K1) rem K2' -> `V rem K2' when
%%% K2 =/= 0, K1 =/= 0 and the compile-time literal test `K1 rem K2 =:= 0'
%%% holds. Exact because `K1' is then a multiple of `K2', so
%%% `V = K2*m + (V rem K1)' for some integer m, i.e. `V' and `V rem K1' are
%%% congruent modulo K2; both `rem' results have the sign of their dividend
%%% (V and `V rem K1' have the same sign, or the intermediate is zero) and a
%%% magnitude below |K2|, so the two remainders are the same integer. `K1 =/= 0'
%%% is required because `V rem 0' raises `badarith' - a chain through a zero
%%% divisor must not be folded away. Exception preserving: `rem' never raises on
%%% integers with a nonzero divisor, and the result cannot grow beyond what the
%%% original chain produced (|V rem K2| < |K2|).
%%%
%%% Fusion 6 (c), the multiplication/division chain:
%%% `(V * K1) div K2' -> `V * (K1 div K2)' when K2 =/= 0,
%%% `K1 rem K2 =:= 0' (so `K1 div K2' is exact) and V is proven to be an
%%% integer of provably BOUNDED magnitude. Exact: with `K1 = K2*q', for any
%%% integer Y the truncated quotient `(Y*K2) div K2' is Y, so
%%% `(V*K1) div K2 = (V*K2*q) div K2 = V*q'. The bound is what makes it
%%% exception preserving: the original chain computes the intermediate
%%% product `V*K1', and a rewrite that replaces it by a product with a smaller
%%% magnitude would silently drop the `system_limit' the original raised for an
%%% operand large enough to exceed the arithmetic size limit. So the fold is
%%% only applied when `V' has a FINITE integer range (the discipline of
%%% `beam_ssa_loop_reduce:bounded/3': a `#t_integer{elements={Lo,Hi}}' with
%%% integer bounds, which `beam_bounds' caps at 2^128) *and* `|K1| < 2^128',
%%% which together bound the intermediate by 2^256 - astronomically below any
%%% arithmetic size limit. Nothing is rewritten when that bound cannot be
%%% proven.
%%%
%%% SHARED SOUNDNESS ARGUMENT
%%%
%%%   * The operand `V' must be proven an integer by the type analysis
%%%     (`#t_integer{}' on the first argument of the first operation). This is
%%%     what makes both constants integers, `V' an integer before and after the
%%%     rewrite, and every `div'/'rem' operand a valid one. Without the proof
%%%     (a float or untyped operand) nothing is rewritten: `*' is not
%%%     associative for floats and `div'/'rem' raise `badarith' for them.
%%%   * Given an integer V, the first operation of every pair is non-raising
%%%     for every constant the fold accepts: `*' never raises, and `div'/'rem'
%%%     only raise for a zero divisor, which `fold/5' rejects (K1 is nonzero for
%%%     every pair whose first operation is `div' or `rem'). The second
%%%     operation is non-raising for the same reason (K2 is nonzero for every
%%%     pair whose second operation is `div' or `rem'; for `*' it cannot raise
%%%     at all). So all three instructions - the two original ones and the
%%%     fused one - provably raise nothing, and the set of exceptions is
%%%     unchanged: no raise is added, and none is removed - not even for the
%%%     `{succeeded,_}' guard the rewrite reuses, which is why rewriting the
%%%     guard to test the fused instruction is sound (it amounts to what the
%%%     type analysis does when it proves a guard cannot fail).
%%%   * The intermediate `D1' must not be used anywhere else (`use_count/2'):
%%%     it is the value the rewrite eliminates, and every other use would
%%%     observe it. Its uses are exactly the second operation - plus, in the
%%%     two-block shape, the first operation's own guard, which the rewrite
%%%     re-points at the fused instruction.
%%%   * Two-block shape only: LSucc must have exactly one predecessor (LA), so
%%%     the definition of `D2' that moves into LA still dominates every one of
%%%     its former uses (LA dominates everything LSucc dominated).
%%%   * Same-block shape only: the two operations must be adjacent instructions
%%%     of one block, so that placing the fused one where the first one was
%%%     cannot reorder anything. Nothing but that block's own instruction list
%%%     changes: no block is added or removed, no branch is rewired, and every
%%%     phi node stays valid, so no dominance question arises there.
%%%   * The source location of the first operation (and of the guard) is kept,
%%%     so error positions and stack traces are unchanged, and the type
%%%     annotations of the rewritten instruction are dropped, since the value
%%%     it now defines is a different one; the type passes recompute them on
%%%     the next iteration.
%%%   * Every uncertain case - a chain whose intermediate is used elsewhere, a
%%%     successor that is not exclusively reached from the chain head, an
%%%     operand whose integrality (or, for (c), whose bound) cannot be proven,
%%%     a chain whose constants do not divide, a `div'/'rem' by zero, a phi node
%%%     that the rewrite would invalidate - is left untouched.
%%%
%%% ARITHMETIC CHAINS THAT ARE DELIBERATELY NOT FUSED
%%%
%%%   * `X * C1 div C2' when C2 does NOT divide C1: NOT exact. Integer division
%%%     truncates, so the rewrite only preserves the value when C2 divides C1
%%%     exactly; `5 * 3 div 2' is 7 while `5 * (3 div 2)' is 5. This is why
%%%     Fusion 6 (c) requires `C1 rem C2 =:= 0' and declines otherwise (the
%%%     divisibility is then provable, since both constants are literals).
%%%   * `(X * C1) rem C2' -> `C1 * (X rem C2/C1)': exact when `C2 rem C1 =:= 0'
%%%     (then `X*C1' and `C1*(X rem (C2 div C1))' are congruent modulo C2 and of
%%%     the same sign and magnitude), but it saves no instruction at all - it
%%%     trades a multiplication for a multiplication - so there is nothing to
%%%     gain and it is not implemented.
%%%   * A chain whose first operand is the *constant*: `C1 * X div C2',
%%%     `(C1 * X) * C2', `C1 rem X rem ...' (where the variable is the divisor,
%%%     so nothing can be folded). Only the shape `bif:Op(V, K)' with the
%%%     variable first and a literal second is matched, exactly as Fusion 4 has
%%%     always required; the reversed order is left untouched.
%%%   * Nested `bsl'/`bsr' shifts, e.g. `(X bsl A) bsr B' -> `X bsl (A - B)'.
%%%     `bsl' is "multiply by 2^Shift" and shifts compose additively, but
%%%     `erts_bsl' raises `system_limit' for an absurd shift count, so an
%%%     unbounded shift operand or count may not be rewritten; a *shift* chain
%%%     of two literal shifts is already folded by `sys_core_fold' before this
%%%     pass runs, and a chain of multiplication constants is the loop pass's
%%%     and this module's job, so there is nothing left for it here.
%%%   * `get_map_element' chains: `D1 = map_get(K1, M), D2 = map_get(K2, D1)'
%%%     and friends. There is no fused BEAM instruction for a map lookup whose
%%%     operand is the result of another map lookup, and `map_get/2' on a value
%%%     that is not a map raises `{badmap,Term}', so nothing can be folded into
%%%     a single instruction. Out of scope.
%%%
-module(beam_ssa_fus_arith).
-moduledoc false.

-export([fuse_blk/3, candidate/2,
         preds/2, has_phi/2, use_count/2, anno_arg_type/1, is_map_type/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").

%%============================================================================
%% Driver
%%============================================================================

%% Entry point of the arithmetic chain fusions, called by
%% `beam_ssa_fus:fuse_blk/3' exactly like that module's own map fusions.
-spec fuse_blk(beam_ssa:label(), beam_ssa:b_blk(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse_blk(LA, Blk, Blocks) ->
    case match(Blk, Blocks) of
        false -> no;
        Chain -> fuse(Chain, LA, Blocks)
    end.

%% Eligibility precondition for the arithmetic chains, used by
%% `beam_ssa_fus:has_candidate/1', which is the eligibility gate the opt-in
%% per-pass budget/watchdog driver (`beam_ssa_opt_budget', wired in
%% `beam_ssa_opt:gate/1') consults. True when the block is the head of a chain
%% of a supported shape whose fold preconditions already hold - either the
%% two-block shape or the same-block one (see the moduledoc).
%%
%% This is a NECESSARY condition, and exactly as strong as it needs to be: the
%% rewrite is only ever attempted for a chain `match/2' recognizes and `fold/5'
%% accepts, so a false gate means the pass could not have changed anything.
%% Whether the chain can *actually* be rewritten (its intermediate must not be
%% used elsewhere, its successor must be exclusively reached from the head, the
%% tail shape must be one of the two supported ones, ...) is decided by the
%% rewrite itself.
-spec candidate(beam_ssa:b_blk(), beam_ssa:block_map()) -> boolean().
candidate(Blk, Blocks) ->
    case match(Blk, Blocks) of
        false ->
            false;
        #{op1 := Op1Set, op1name := Op1, op2name := Op2, k1 := K1, k2 := K2} ->
            fold(Op1, Op2, K1, K2, anno_arg_type(Op1Set)) =/= false
    end.

%% Match the chain head at `Blk'. Returns a map describing the pieces of the
%% shape it matched (see the moduledoc), or `false' when `Blk' is not the head
%% of a chain of a supported shape. The two-block shapes are tried first, then
%% the same-block one.
%%
%% A chain of a given pair of instructions is described by exactly one shape
%% here, so the order only decides which *pair* is folded first when a block is
%% the head of more than one chain.
-spec match(beam_ssa:b_blk(), beam_ssa:block_map()) -> false | map().
match(Blk, Blocks) ->
    case match_two_block(Blk, Blocks) of
        false ->
            case match_no_guard(Blk, Blocks) of
                false -> match_same_block(Blk);
                Chain -> Chain
            end;
        Chain -> Chain
    end.

%% The two-block shape of the moduledoc.
-spec match_two_block(beam_ssa:b_blk(), beam_ssa:block_map()) -> false | map().
match_two_block(#b_blk{is=Is,last=#b_br{bool=Bool,succ=LSucc,fail=LFail}=BrA},
                Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op={succeeded,_},dst=Bool,args=[#b_var{}=D1]}=Guard,
         #b_set{op={bif,Op1},dst=D1,
                args=[#b_var{}=V,#b_literal{val=K1}]}=Op1Set | RevPrefix]
            when is_integer(K1) ->
            case Blocks of
                #{LSucc := #b_blk{is=[#b_set{op={bif,Op2},
                                             args=[D1,#b_literal{val=K2}]}=Op2Set
                                         | Rest],
                                  last=Term}}
                    when is_integer(K2) ->
                    case fold_pair(Op1, Op2) of
                        true ->
                            D2 = Op2Set#b_set.dst,
                            Shape = #{prefix => lists:reverse(RevPrefix),
                                      guard => Guard,
                                      op1 => Op1Set,
                                      op1name => Op1,
                                      op2 => Op2Set,
                                      op2name => Op2,
                                      dst1 => D1,
                                      dst2 => D2,
                                      var => V,
                                      k1 => K1,
                                      k2 => K2,
                                      bool => Bool,
                                      lsucc => LSucc,
                                      lfail => LFail,
                                      rest => Rest,
                                      term => Term,
                                      br => BrA},
                            classify(Rest, Term, D2, Shape);
                        false ->
                            false
                    end;
                #{} ->
                    false
            end;
        _ ->
            false
    end;
match_two_block(_Blk, _Blocks) ->
    false.

%% The guard-free two-block shape of the moduledoc: block LA ends with the first
%% operation alone and branches to LSucc *unconditionally*, and LSucc starts
%% with the second operation. This is the shape left behind when the type
%% analysis has proven the FIRST operation non-raising: it folds the
%% `{succeeded,_}' marker to the literal `true' (so the block no longer has the
%% guard `match_two_block/2' looks for) and makes both edges of the branch point
%% at LSucc, while the two blocks themselves are only merged much later, by
%% `ssa_opt_merge_blocks' in the late epilogue - i.e. after this pass has run.
%%
%% This shape is what makes Fusion 6 (c) fire. Its non-raising precondition is
%% the very bound `fold/5' requires of (c) (see `bounded_operand/1' and
%% `beam_call_types:succeeds_if_smallish/1'), so whenever (c) is applicable the
%% guard of the first operation is already gone by the time this pass runs -
%% neither the guarded form of the two-block shape nor the same-block shape
%% (which needs the merge that has not happened yet) can describe it.
%%
%% Only the branch is required to be unconditional (both edges to LSucc): the
%% rewrite moves the definition of the second operation into LA, and an
%% unconditional edge is what makes that unconditionally safe. The rest of the
%% conditions are checked by `fuse_no_guard/5'.
-spec match_no_guard(beam_ssa:b_blk(), beam_ssa:block_map()) -> false | map().
match_no_guard(#b_blk{is=Is,last=#b_br{succ=LSucc,fail=LSucc}=BrA}=Blk, Blocks) ->
    case lists:reverse(Is) of
        [#b_set{op={bif,Op1},dst=D1,
                args=[#b_var{}=V,#b_literal{val=K1}]}=Op1Set | RevPrefix]
            when is_integer(K1) ->
            case Blocks of
                #{LSucc := #b_blk{is=[#b_set{op={bif,Op2},
                                             args=[D1,#b_literal{val=K2}]}=Op2Set
                                         | Rest],
                                  last=Term}}
                    when is_integer(K2) ->
                    case fold_pair(Op1, Op2) of
                        true ->
                            D2 = Op2Set#b_set.dst,
                            #{blk => Blk,
                              prefix => lists:reverse(RevPrefix),
                              op1 => Op1Set,
                              op1name => Op1,
                              op2 => Op2Set,
                              op2name => Op2,
                              dst1 => D1,
                              dst2 => D2,
                              var => V,
                              k1 => K1,
                              k2 => K2,
                              lsucc => LSucc,
                              lfail => LSucc,
                              rest => Rest,
                              term => Term,
                              br => BrA,
                              tail => no_guard};
                        false ->
                            false
                    end;
                #{} ->
                    false
            end;
        _ ->
            false
    end;
match_no_guard(_Blk, _Blocks) ->
    false.

%% The same-block shape of the moduledoc: two adjacent instructions of one
%% block, `D1 = bif:Op1(V, K1)' immediately followed by `D2 = bif:Op2(D1, K2)'.
%% The pair is looked for anywhere in the block, since nothing about it depends
%% on the block's terminator, its successors or its position in the function.
%%
%% This is what lets the folds fire on a chain the type analysis has already
%% proven non-raising throughout: for those, both operations live in one block
%% and the guard that used to make the two-block shape recognizable has been
%% folded to `true' and dropped (see the moduledoc).
-spec match_same_block(beam_ssa:b_blk()) -> false | map().
match_same_block(#b_blk{is=Is}=Blk) ->
    match_same_block_is(Is, [], Blk).

%% Scan the block for an adjacent foldable pair. Only the *shape* is decided
%% here (`fold_pair/2'); whether the fold itself is provable is left to
%% `fold/5', exactly as in the two-block path, and both the first and the second
%% constant must be integer literals with the variable first, the same operand
%% order requirement the two-block matcher has always imposed.
-spec match_same_block_is([beam_ssa:b_set()], [beam_ssa:b_set()],
                          beam_ssa:b_blk()) -> false | map().
match_same_block_is([#b_set{op={bif,Op1},dst=D1,
                            args=[#b_var{}=V,#b_literal{val=K1}]}=Op1Set,
                     #b_set{op={bif,Op2},dst=D2,
                            args=[D1,#b_literal{val=K2}]}=Op2Set | Rest],
                    RevPrefix, Blk)
  when is_integer(K1), is_integer(K2), D1 =/= D2 ->
    case fold_pair(Op1, Op2) of
        true ->
            #{blk => Blk,
              prefix => lists:reverse(RevPrefix),
              op1 => Op1Set,
              op1name => Op1,
              op2 => Op2Set,
              op2name => Op2,
              dst1 => D1,
              dst2 => D2,
              var => V,
              k1 => K1,
              k2 => K2,
              rest => Rest,
              tail => same_block};
        false ->
            %% Not a foldable pair; the second operation may still start one.
            match_same_block_is([Op2Set | Rest], [Op1Set | RevPrefix], Blk)
    end;
match_same_block_is([Set | Is], RevPrefix, Blk) ->
    match_same_block_is(Is, [Set | RevPrefix], Blk);
match_same_block_is([], _RevPrefix, _Blk) ->
    false.

%% The tail shapes of the two-block shape, decided by looking at the
%% instructions behind the second operation and at its block's terminator. A
%% single instruction behind it is its guard, which must then also be the
%% block's branch (the shape of Fusion 4); with nothing behind it, the second
%% operation is alone and the block is kept as an (emptied) jump block. Any
%% other tail is rejected here, so that the rewrites can rely on the pieces
%% they match.
-spec classify([beam_ssa:b_set()], beam_ssa:terminator(), beam_ssa:b_var(),
               map()) -> false | map().
classify([#b_set{op={succeeded,_},dst=B2,args=[D2]}],
         #b_br{bool=B2,succ=LFinal,fail=LFail2}, D2, Shape) ->
    Shape#{tail => guarded, guard2 => B2,
           l_final => LFinal, l_fail2 => LFail2};
classify([], _Term, _D2, Shape) ->
    Shape#{tail => unguarded};
classify(_Rest, _Term, _D2, _Shape) ->
    false.

%% The fold is possible (or not) in principle; check the conditions that depend
%% on the tail shape of the chain and rewrite it.
-spec fuse(map(), beam_ssa:label(), beam_ssa:block_map()) ->
          no | {yes, beam_ssa:block_map()}.
fuse(#{op1 := Op1Set, op1name := Op1, op2name := Op2, k1 := K1, k2 := K2}=Chain,
     LA, Blocks) ->
    case fold(Op1, Op2, K1, K2, anno_arg_type(Op1Set)) of
        false ->
            no;
        {FusedOp,FusedK} ->
            case maps:get(tail, Chain) of
                guarded -> fuse_guarded(Chain, FusedOp, FusedK, LA, Blocks);
                unguarded -> fuse_unguarded(Chain, FusedOp, FusedK, LA, Blocks);
                no_guard -> fuse_no_guard(Chain, FusedOp, FusedK, LA, Blocks);
                same_block -> fuse_same_block(Chain, FusedOp, FusedK, LA, Blocks)
            end
    end.

%%============================================================================
%% Guarded tail: LSucc is exactly [D2 = Op2(D1, K2), B2 = succeeded(D2)] and
%% branches on B2. This is Fusion 4's shape.
%%============================================================================

fuse_guarded(#{prefix := Prefix, guard := Guard, op1 := Op1Set, dst1 := D1,
               dst2 := D2, var := V, lsucc := LSucc, lfail := LFail, br := BrA,
               guard2 := B2, l_final := LFinal, l_fail2 := LFail2},
             FusedOp, FusedK, LA, Blocks) ->
    Type = anno_arg_type(Op1Set),
    case guarded_ok(LA, LSucc, LFinal, LFail2, D1, B2, Type, Blocks) of
        true ->
            Fused = fused_op(Op1Set, FusedOp, FusedK, D2, V),
            %% The guard of the first operation now tests the fused
            %% instruction, and the branch keeps the same failure edge (LFail).
            Br = BrA#b_br{succ=LFinal, fail=LFail},
            Guard1 = Guard#b_set{args=[D2]},
            A1 = #b_blk{is=Prefix ++ [Fused,Guard1], last=Br},
            {yes,maps:remove(LSucc, Blocks#{LA => A1})};
        false ->
            no
    end;
%% Only reachable if classify/4 ever describes a tail it does not hand to this
%% function: decline rather than rewrite something unproven.
fuse_guarded(_Chain, _FusedOp, _FusedK, _LA, _Blocks) ->
    no.

%% All the conditions that must hold for the guarded rewrite to be provably
%% valid. They are the conditions of the original Fusion 4, generalised: the
%% second operation may be a different BIF, but it is eliminated in exactly the
%% same way and its guard's destination B2 is consumed only by the branch that
%% disappears with it.
-spec guarded_ok(beam_ssa:label(), beam_ssa:label(), beam_ssa:label(),
                 beam_ssa:label(), beam_ssa:b_var(), beam_ssa:b_var(),
                 false | term(), beam_ssa:block_map()) -> boolean().
guarded_ok(LA, LSucc, LFinal, LFail2, D1, B2, Type, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    %% `*' is exact for integers but not associative for floats, `div'/`rem'
    %% are only defined for integers, so the operand must be proven to be an
    %% integer. This is what makes both constants integers and the fused
    %% instruction a valid replacement.
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
    %% operation, and is the exception block for a guard failure.
    andalso lists:all(fun(L) -> L =/= Special end, [LA,LSucc,LFinal])
    %% D1 is the intermediate the rewrite eliminates. It may only be used by
    %% the first operation's own guard and by the second operation; any other
    %% use would observe the eliminated value.
    andalso use_count(D1, Blocks) =:= 2
    %% The guard of the removed operation is used only by its branch.
    andalso use_count(B2, Blocks) =:= 1
    %% Phi nodes would have to be rewritten when the predecessors change; in
    %% particular LFail2 becomes unreachable, so its phi would keep an entry
    %% for a block that no longer exists. It is cheaper (and safer) to simply
    %% not fuse in that case.
    andalso not has_phi(LSucc, Blocks)
    andalso not has_phi(LFinal, Blocks)
    andalso not has_phi(LFail2, Blocks).

%%============================================================================
%% Unguarded tail: the second operation is the only instruction of its block,
%% so its `{succeeded,_}' guard has already been folded away by the type
%% analysis. The block structure is kept and LSucc is left as an emptied jump
%% block, which a later pass merges away.
%%============================================================================

fuse_unguarded(#{prefix := Prefix, guard := Guard, op1 := Op1Set, dst1 := D1,
                 dst2 := D2, var := V, lsucc := LSucc, rest := Rest, br := BrA,
                 term := Term},
               FusedOp, FusedK, LA, Blocks) ->
    Type = anno_arg_type(Op1Set),
    case unguarded_ok(LA, LSucc, D1, Type, Blocks) of
        true ->
            Fused = fused_op(Op1Set, FusedOp, FusedK, D2, V),
            Guard1 = Guard#b_set{args=[D2]},
            A1 = #b_blk{is=Prefix ++ [Fused,Guard1], last=BrA},
            %% Everything behind the removed operation, and the terminator,
            %% stay exactly where they were; only the definition of D2 has
            %% moved (it keeps its name, so this is not observable).
            Succ = #b_blk{is=Rest, last=Term},
            {yes, Blocks#{LA => A1, LSucc => Succ}};
        false ->
            no
    end;
%% As in fuse_guarded/5: decline rather than rewrite something unproven.
fuse_unguarded(_Chain, _FusedOp, _FusedK, _LA, _Blocks) ->
    no.

%% All the conditions that must hold for the unguarded rewrite to be provably
%% valid. Fewer than for the guarded one: the block structure, the successors
%% of every block and their phi nodes are all unchanged, so only the integrality
%% of the operand, the single predecessor of LSucc and the uses of D1 matter.
-spec unguarded_ok(beam_ssa:label(), beam_ssa:label(), beam_ssa:b_var(),
                   false | term(), beam_ssa:block_map()) -> boolean().
unguarded_ok(LA, LSucc, D1, Type, Blocks) ->
    is_integer_type(Type)
    andalso preds(LSucc, Blocks) =:= [LA]
    andalso LSucc =/= LA
    %% As in the guarded case: the intermediate may only be used by the first
    %% operation's own guard and by the second operation - in particular not by
    %% the part of LSucc that stays behind or by its terminator, which would
    %% then observe the eliminated value.
    andalso use_count(D1, Blocks) =:= 2.

%%============================================================================
%% Guard-free two-block tail: LA ends with the first operation alone and
%% branches to LSucc unconditionally (its `{succeeded,_}' guard has been folded
%% away by the type analysis), and LSucc starts with the second operation. As in
%% the unguarded tail, the block structure is kept; unlike it, there is no guard
%% left in LA to re-point at the fused instruction.
%%============================================================================

fuse_no_guard(#{blk := Blk, prefix := Prefix, op1 := Op1Set, dst1 := D1,
                dst2 := D2, var := V, lsucc := LSucc, rest := Rest},
              FusedOp, FusedK, LA, Blocks) ->
    case no_guard_ok(LA, LSucc, D1, Blocks) of
        true ->
            Fused = fused_op(Op1Set, FusedOp, FusedK, D2, V),
            %% The fused instruction takes the place (and the name, position and
            %% source location) of the first operation. LA's terminator already
            %% leads to LSucc on both edges, so it is left exactly as it is; only
            %% the second operation leaves LSucc, whose own terminator and whose
            %% instructions behind the removed one stay untouched (the definition
            %% of D2 keeps its name, so its former uses are not affected).
            A1 = Blk#b_blk{is=Prefix ++ [Fused]},
            #b_blk{}=Blk2 = maps:get(LSucc, Blocks),
            Succ = Blk2#b_blk{is=Rest},
            {yes, Blocks#{LA => A1, LSucc => Succ}};
        false ->
            no
    end;
%% As in the other rewrites: decline rather than rewrite something unproven.
fuse_no_guard(_Chain, _FusedOp, _FusedK, _LA, _Blocks) ->
    no.

%% The conditions that must hold for the guard-free rewrite to be provably
%% valid. Beyond what `fold/5' has already established (the integrality of `V'
%% and the properties of the two constants, read from the first operation) there
%% are three:
%%
%%   * LSucc is exclusively reached from LA (`preds/2'), so the definition of
%%     `D2' that moves into LA still dominates every one of its former uses -
%%     both in the instructions of LSucc that stay behind and in everything
%%     LSucc dominates.
%%   * LSucc is not LA itself, which would make the two block updates below
%%     refer to the same block and lose one of them.
%%   * `D1' is used exactly once in the whole function (`use_count/2'): its only
%%     use is the second operation, which the rewrite removes. Any other use -
%%     in another instruction of LA (in particular its terminator, which would
%%     be a branch on the intermediate), in LSucc or in a phi node - would
%%     observe the eliminated value.
%%
%% The branch that carries the chain is already known to be unconditional, and
%% neither LA nor LSucc may be the exception pseudo block: taking a definition
%% into or out of it is not an ordinary block rewrite.
-spec no_guard_ok(beam_ssa:label(), beam_ssa:label(), beam_ssa:b_var(),
                  beam_ssa:block_map()) -> boolean().
no_guard_ok(LA, LSucc, D1, Blocks) ->
    Special = ?EXCEPTION_BLOCK,
    LSucc =/= LA
    andalso LSucc =/= Special
    andalso LA =/= Special
    andalso preds(LSucc, Blocks) =:= [LA]
    andalso use_count(D1, Blocks) =:= 1.

%%============================================================================
%% Same-block tail: the two operations are adjacent instructions of the matched
%% block, because the type analysis has already folded away the guard of the
%% first one (and merged the two blocks). The rewrite touches that block's
%% instruction list and nothing else.
%%============================================================================

fuse_same_block(#{blk := Blk, prefix := Prefix, op1 := Op1Set, dst1 := D1,
                  dst2 := D2, var := V, rest := Rest},
                FusedOp, FusedK, LA, Blocks) ->
    case use_count(D1, Blocks) =:= 1 of
        true ->
            Fused = fused_op(Op1Set, FusedOp, FusedK, D2, V),
            %% The pair `[Op1Set, Op2Set]' is replaced by the single fused
            %% instruction, in the position of the first operation and under
            %% the name of the second one; the terminator and the block's other
            %% instructions (including anything between the pair and the
            %% terminator) are untouched. `D1' - used nowhere else - becomes
            %% dead and its definition is gone, so no dead instruction is left
            %% behind.
            A1 = Blk#b_blk{is=Prefix ++ [Fused | Rest]},
            {yes, Blocks#{LA => A1}};
        false ->
            no
    end;
%% As in fuse_guarded/5 and fuse_unguarded/5: only reachable if match/2 ever
%% hands over a shape this function does not describe (a same-block chain whose
%% intermediate has more than one use is simply declined).
fuse_same_block(_Chain, _FusedOp, _FusedK, _LA, _Blocks) ->
    no.

%% There is exactly one condition to check for the same-block rewrite, and it is
%% the reason `D1' may be eliminated: the intermediate is used exactly once in
%% the whole function, that use being the second operation. Nothing else can
%% observe it - no other instruction of this block (before the pair, between it
%% and the terminator, or in the terminator itself), no instruction of another
%% block and no phi node, which `use_count/2' counts as a use like any other.
%%
%% Everything else the fold needs was already checked by `fold/5' (the
%% integrality of `V', the nonzero divisors, the divisibility and - for Fusion
%% 6 (c) - the finiteness of `V' and the smallness of the constant), on the
%% very same `arg_types' annotation the two-block path reads. No CFG condition
%% is needed here: the rewrite does not move a definition across a block
%% boundary, does not remove or add a block, and does not rewire a branch, so
%% the dominance of `D2' and every phi node keep holding as they did.
%%============================================================================
%% The folds
%%============================================================================

%% The fused instruction: it takes the position (and the source location) of
%% the first operation of the chain, but its type annotations are dropped,
%% because the value it defines is a different one; the type passes recompute
%% them on the next iteration.
-spec fused_op(beam_ssa:b_set(), atom(), integer(), beam_ssa:b_var(),
               beam_ssa:b_var()) -> beam_ssa:b_set().
fused_op(Op1Set, FusedOp, FusedK, D2, V) ->
    Anno = maps:without([arg_types,result_type], Op1Set#b_set.anno),
    #b_set{op={bif,FusedOp}, dst=D2, args=[V,#b_literal{val=FusedK}], anno=Anno}.

%% The pairs of arithmetic BIFs a constant chain may consist of, in order: the
%% first operation survives and is applied to the folded constant, the second
%% one disappears.
-spec fold_pair(atom(), atom()) -> boolean().
fold_pair('*', '*') -> true;                    %Fusion 4
fold_pair('div', 'div') -> true;                %Fusion 6 (a)
fold_pair('rem', 'rem') -> true;                %Fusion 6 (b)
fold_pair('*', 'div') -> true;                  %Fusion 6 (c)
fold_pair(_, _) -> false.

%% The fold of a chain: the fused BIF and the constant it takes - or `false'
%% when the pair is not foldable. `Type' is the type of the first operand `V',
%% as recorded on the first operation by the type analysis.
%%
%% The conditions are grouped per pair, and each of them is what makes the fold
%% exact *and* exception preserving; see the moduledoc for the reasoning. In
%% short: the operand must be an integer, a `div'/`rem' divisor must be
%% nonzero, the two constants must divide exactly where the value requires it,
%% and a fold that removes a growing intermediate product must prove that
%% product bounded.
-spec fold(atom(), atom(), integer(), integer(), false | term()) ->
          false | {atom(), integer()}.
fold('*', '*', K1, K2, Type) ->
    %% (V * K1) * K2 -> V * (K1 * K2): exact for integers.
    case is_integer_type(Type) of
        true -> {'*', K1 * K2};
        false -> false
    end;
fold('div', 'div', K1, K2, Type) ->
    %% (V div K1) div K2 -> V div (K1 * K2): exact for a nonzero product, and
    %% the product must be nonzero for `div' (and for the original `V div K1').
    case is_integer_type(Type) andalso K1 * K2 =/= 0 of
        true -> {'div', K1 * K2};
        false -> false
    end;
fold('rem', 'rem', K1, K2, Type) ->
    %% (V rem K1) rem K2 -> V rem K2: exact when K1 is a multiple of K2. Both
    %% divisors must be nonzero ("`V rem 0'" raises `badarith'), and it is the
    %% divisibility of the two literals that makes the fold provable.
    case is_integer_type(Type) andalso K1 =/= 0 andalso K2 =/= 0
        andalso K1 rem K2 =:= 0 of
        true -> {'rem', K2};
        false -> false
    end;
fold('*', 'div', K1, K2, Type) ->
    %% (V * K1) div K2 -> V * (K1 div K2): exact when K2 divides K1 exactly.
    %% `V * K1' is the intermediate the original chain builds, so the fold also
    %% requires that it is provably bounded (see `bounded_operand/1'): without
    %% that proof a rewrite that shrinks the intermediate could silently drop
    %% the `system_limit' the original raised for a huge operand.
    case is_integer_type(Type) andalso K2 =/= 0 andalso K1 rem K2 =:= 0
        andalso bounded_operand(Type) andalso small_constant(K1) of
        true -> {'*', K1 div K2};
        false -> false
    end;
fold(_Op1, _Op2, _K1, _K2, _Type) ->
    false.

%% True when the type analysis has proven a finite integer range for the
%% operand. This is the discipline of `beam_ssa_loop_reduce:bounded/3': only a
%% two-integer range proves a bound (`beam_bounds' caps its ranges at 2^128 and
%% reports anything beyond a cap as an infinity), while `elements=any' and the
%% literal/union forms prove nothing.
-spec bounded_operand(false | term()) -> boolean().
bounded_operand(#t_integer{elements={Lo,Hi}}) ->
    is_integer(Lo) andalso is_integer(Hi)
        andalso abs(Lo) bsr 128 =:= 0 andalso abs(Hi) bsr 128 =:= 0;
bounded_operand(_) ->
    false.

%% A constant that is itself below 2^128, the same bound as `bounded_operand/1',
%% so that the intermediate product of the bound proof for Fusion 6 (c) stays
%% below 2^256 even for a huge literal constant.
-spec small_constant(integer()) -> boolean().
small_constant(K) ->
    abs(K) bsr 128 =:= 0.

%%============================================================================
%% The SSA vocabulary shared with `beam_ssa_fus'
%%============================================================================

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

%% True when the type analysis has proven the value to be an integer. Only
%% integers may be folded: `*' is exact and associative for them, while it is
%% neither for floats, and `div'/`rem' are not defined for them at all.
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
