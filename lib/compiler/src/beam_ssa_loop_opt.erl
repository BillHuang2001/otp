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
%%% 3. Induction-variable canonicalization. A loop-carried variable of the
%%%    loop header (a phi with at least one incoming value from outside the
%%%    loop and at least one from inside it) whose values on the inside
%%%    edges are all `V + C', `C + V' or `V - C' with the same integer
%%%    literal C -- or `V' itself, i.e. step zero -- and whose values on the
%%%    outside edges are all integers, is described by the canonical pair
%%%    {Init, Step}. From that description the pass *proves*, by induction
%%%    over the iterations, that the variable is an integer, and -- when the
%%%    entry value is non-negative and the step is non-negative -- that it
%%%    is non-negative as well. The same two facts are propagated through
%%%    the integer-valued operations inside the loop, so a value computed
%%%    from an induction variable is described too.
%%%
%%%    The induction variable is not otherwise rewritten: nothing is gained
%%%    by writing `V + (-1)' for `V - 1', and re-associating or hoisting the
%%%    recurrence would change when it is evaluated. The canonical
%%%    description exists to licence the rewrites of item 4.
%%%
%%%    The one canonicalisation that *is* an IR rewrite is the operand order
%%%    of a commutative arithmetic instruction: `C + V' and `C * V' (a
%%%    literal on the left, a variable on the right) become `V + C' and
%%%    `V * C', so that every recurrence has a single syntactic shape. `+'
%%%    and `*' are commutative for every number and fail on the same
%%%    non-number operands, so neither a value nor an exception changes.
%%%
%%% 4. Strength reduction. An arithmetic instruction is rewritten *in place*
%%%    (same block, same destination) into a cheaper operation, and only
%%%    when the rewrite is exact for every value its operands can have:
%%%
%%%    * `X * 2^n' (n >= 1, `2^n' a literal) becomes `X bsl n' when X is
%%%      provably an integer. For integers -- including negative ones and
%%%      bignums -- `bsl' is exactly multiplication by a power of two.
%%%    * `X * 2' becomes `X + X' otherwise, which is exact for every
%%%      number: Erlang doubles a float without rounding, so `X + X' and
%%%      `X * 2' are the same for floats as well as for integers.
%%%    * `X div 2^n' (n >= 1) becomes `X bsr n' when X is provably a
%%%      NON-NEGATIVE integer. `div' truncates toward zero while `bsr'
%%%      floors, and the two agree exactly for a non-negative dividend; for
%%%      a dividend that may be negative the rewrite is skipped.
%%%
%%%    Everything else is left alone, in particular a multiplication or a
%%%    division by a literal that is not a power of two, `X div 0' (which
%%%    must keep raising) and every operation with an operand whose
%%%    integrality/non-negativity cannot be proved.
%%%
%%%    Integrality is proved from two independent sources, and either one is
%%%    enough:
%%%
%%%    * the *structural* facts of item 3, i.e. integer literals, integer
%%%      induction variables and the integer-valued operations built from
%%%      them (the fixed point in `value_kinds/2');
%%%    * the *type annotations* that `beam_ssa_type' leaves on each
%%%      instruction (`anno = #{arg_types := #{Index => #type{}}}', the type
%%%      of the operand at the 0-based `Index'). An operand annotated
%%%      `#t_integer{}' is an integer, and an operand annotated
%%%      `#t_integer{elements={Lo,_}}' with `Lo >= 0' is non-negative. This
%%%      is what makes the pass fire on real code, where the interesting
%%%      operands are function arguments and values extracted from a message
%%%      or a term -- values that have no defining instruction the structural
%%%      analysis could classify.
%%%
%%%    A type annotation is only ever trusted for the instruction it is
%%%    attached to, because `beam_ssa_type' refines a variable along the
%%%    edges it flows down: the type of a variable at one use says nothing
%%%    about its type at another. The annotation is therefore consulted where
%%%    the operand is used, and the structural facts are the fallback.%%%
%%% CORRECTNESS RULES (conservative by design -- when in doubt, do nothing)
%%%
%%% * Only `#b_set{}' value instructions are considered; phis, terminators,
%%%   calls and side-effecting instructions are never touched.
%%% * Only instructions on an explicit whitelist are hoisted: the
%%%   constructors in ?SAFE_OPS and the tag tests in ?SAFE_BIFS. Both are
%%%   side-effect free AND can never raise for any operand values. This is
%%%   stricter than `beam_ssa:no_side_effect/1', which is only a "can be
%%%   omitted if the result is unused" predicate and returns true for
%%%   instructions that can still fail (get_hd, get_tuple_element, ...).
%%%   Hoisting an instruction that may raise would execute it on paths where
%%%   it was not executed before, so those are deliberately excluded. The
%%%   whitelist is further restricted to instructions that also have a
%%%   block-independent code generation rule; see the comments on ?SAFE_OPS
%%%   and ?SAFE_BIFS below.
%%% * A tag test such as `is_reference/1' is the interesting case for this
%%%   pass, because guard code inside a receive loop is the only place where
%%%   a loop-invariant test reliably shows up in the CFG at this point (see
%%%   below). The tests are constant-tag comparisons, so hoisting one out of
%%%   the loop removes it from the hot path without changing any result.
%%% * Consequently arithmetic is NOT hoisted here: an operation such as `+'
%%%   or `*' can raise, and moving it into the preheader would execute it on
%%%   paths where it was not executed before. Arithmetic is instead
%%%   *strength-reduced in place* (item 4 above), which never moves an
%%%   instruction to a path that did not have the original.%%% * The strength reducer only ever replaces an operation by one that
%%%   either succeeds and computes the same value, or fails on exactly the
%%%   same operands. Since every rewrite requires a proof that the operands
%%%   are integers (or, for `div' -> `bsr', non-negative integers), the
%%%   operand set on which the new instruction fails is empty, and no new
%%%   exception can appear. In particular the divisor is a literal power of
%%%   two, so no division by zero is ever created and an existing one is
%%%   never removed.
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
%%% REACH: at this pipeline point the CFG of an ordinary tail-recursive
%%% function contains no cycle (self-recursion is still a tail `call'), and a
%%% `receive' loop's header carries no phis, so the loop/IV parts of the pass
%%% find nothing to do in typical Erlang code. The strength reducer, however,
%%% needs neither a phi nor an induction variable: it only needs a proof that
%%% one operand of a product or quotient is an integer, which the SSA type
%%% annotations provide -- a guarded `receive' clause such as
%%% `{n,X} when is_integer(X), X >= 0 -> X * 8 + X div 4' is reduced. The
%%% reducer is applied to the loop body *and* to every block dominated by the
%%% loop header, because the body of a natural loop is defined by reaching
%%% the back edge and a receive clause that returns to its caller (or raises)
%%% does not; see `loop_region/3'.
%%%
%%% * Unrolling. A sound unroller needs an exact trip count, i.e. the bounds
%%%   of a loop-carried variable and a decidable exit test. The pass runs
%%%   after the SSA type analysis, so `#t_integer{elements={Lo,Hi}}' is
%%%   available for the type of a variable *at a use*; deriving a trip count
%%%   from that and rewriting the loop as a counted one is a much larger
%%%   transformation (it needs the exit test to be the loop's only exit and
%%%   the bounds to be loop-invariant). Unrolling is therefore deferred
%%%   rather than guessed.%%% * Accumulator-based strength reduction of a product by a constant that is
%%%   not a power of two (`X * 3' turned into a second loop-carried variable
%%%   incremented by the step times 3). It replaces one multiplication by one
%%%   addition, at the price of a new phi, a new entry computation in the
%%%   preheader and a rewritten use set -- and the extra work is only repaid
%%%   when the number of iterations is large, which is not knowable here.
%%%   A product or quotient by a non-power-of-two constant is therefore left
%%%   alone, as are `rem' by a power of two (there is no cheap exact form for
%%%   a negative dividend), `X * 2^n' with an X that may be a float, and a
%%%   shift by a negative literal.
%%% * Nothing is ever *moved* by the strength reducer; every rewrite keeps
%%%   the instruction in its block and its predecessor set, so no evaluation
%%%   is added on a path that did not have the original operation.
%%% * Only a hit of the split `div' -> `bsr' rule needs the dividend to be
%%%   non-negative; the non-negativity proof is the induction-variable one
%%%   (non-negative entry value and non-negative step), a value built from
%%%   non-negative values by `+', `*', `div', `rem', `band', `bor', `bxor',
%%%   `bsl' or `bsr', or a type annotation with a non-negative lower bound.
%%%   Nothing else is accepted.
-export([opt/1, eligible/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").
-import_record(beam_ssa_opt, [opt_st]).

%% Maximum number of instructions hoisted per loop. Bounds the growth of the
%% preheader on pathological input.
-define(MAX_HOIST, 64).

%% Operations that are side-effect free and that can never raise, for any
%% operand values. Everything not listed here is left in the loop.
%%
%% Purity and non-raising are necessary but NOT sufficient: an instruction
%% that is moved into the preheader leaves its original block, so it must
%% also have a code generation rule that is valid *anywhere*. This excludes
%% the fused type tests `is_tagged_tuple'/3 and `is_nonempty_list'/1
%% (produced by `ssa_opt_record'/1 and by the list pattern builder), because
%% `beam_ssa_codegen' only implements them as block-local tests that must be
%% the last instruction of a block and be consumed immediately by that
%% block's terminator (the `cg_block/3' clauses for is_tagged_tuple and
%% is_nonempty_list). There is no standalone `cg_instr/3' clause for either,
%% so hoisting one into a preheader makes code generation fail
%% (`function_clause' in `cg_instr/3'), and the is_nonempty_list fallback
%% additionally asserts the `was_bif_is_list' annotation that such an
%% instruction does not have. Constructors, on the other hand, are ordinary
%% value instructions (`cg_instr(put_list, ...)' / `cg_instr(put_tuple, ...)')
%% and are safe to move.
-define(SAFE_OPS, [put_list, put_tuple]).

%% `{bif,Name}' value instructions that are additionally hoisted, listed as
%% {Name,Arity} pairs. These are the Erlang tag tests. Each one is a plain
%% tag comparison that is pure and cannot fail for any argument (see the
%% `is_*_1'/1 functions in `erts/emulator/beam/erl_bif_op.c'), and each is
%% declared as `ubif' in `erts/emulator/beam/bif.tab', so
%% `{bif,Name,{f,0},Args,Dst}' is a valid *standalone* instruction
%% (`beam_ssa_codegen:cg_block/3', fallback clause) as well as a block-local
%% test. They are therefore movable into a preheader.
%%
%% The arity is part of the key, because the arity decides whether the
%% instruction can raise:
%%
%% * `is_function/2' shares the name `is_function' with the safe
%%   `is_function/1', but raises `badarg' unless the second argument is a
%%   non-negative integer (`erl_is_function/3' in erl_bif_op.c); v3_core
%%   even forces a call to it in a guard to be protected for that reason.
%% * `is_record/2' and `is_record/3' (i.e. `{bif,is_record}' with two or
%%   three operands) raise `badarg' unless the tag operand is an atom
%%   (`is_record_2/3' in erl_bif_op.c).
%%
%% None of the raising arities are listed, so they stay in the loop.
-define(SAFE_BIFS, [{is_atom,1}, {is_binary,1}, {is_bitstring,1},
                    {is_boolean,1}, {is_float,1}, {is_function,1},
                    {is_integer,1}, {is_list,1}, {is_map,1}, {is_number,1},
                    {is_pid,1}, {is_port,1}, {is_reference,1},
                    {is_tuple,1}]).

%% Arithmetic BIFs whose result is an integer whenever all their operands are
%% integers. Some of them can raise (`div'/`rem' on a zero divisor, `bsl'/
%% `bsr' on a negative shift count or a non-integer operand), which is
%% irrelevant: what is claimed below is only "if this instruction produces a
%% value, that value is an integer", and the operand facts are themselves
%% only claimed for values that were produced. `+'/`-'/'*' also accept
%% floats, but their operands are integers whenever the rule is applied.
-define(INT_BIFS, ['+','-','*','div','rem','band','bor','bxor','bsl','bsr']).

%% The subset of ?INT_BIFS that also preserves non-negativity: the result is
%% >= 0 whenever every operand is >= 0. Subtraction is deliberately absent (it
%% can go negative); so are all float-producing operations.
-define(NAT_BIFS, ['+','*','div','rem','band','bor','bxor','bsl','bsr']).

-spec opt({#opt_st{}, term()}) -> {#opt_st{}, term()}.
opt({#opt_st{ssa=Linear0}=St, FuncDb}) when is_list(Linear0) ->
    %% Any unexpected condition skips the optimization; it must never turn a
    %% compilable module into a compilation error.
    Linear = try
                 opt_linear(Linear0)
             catch
                 _:_ ->
                     Linear0
             end,
    {St#opt_st{ssa=Linear}, FuncDb};
opt({St, FuncDb}) ->
    {St, FuncDb}.

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

opt_linear(Linear0) ->
    Blocks0 = maps:from_list(Linear0),
    Order0 = [L || {L, _} <- Linear0],
    case Order0 of
        [] ->
            Linear0;
        [_ | _] ->
            {Loops, Doms} = find_loops(Order0, Blocks0),
            case Loops of
                [] ->
                    Linear0;
                _ ->
                    Next = lists:max([0 | Order0]) + 1,
                    {Blocks, _} =
                        lists:foldl(fun(Loop, {B, N}) ->
                                            opt_loop(Loop, Order0, Doms, B, N)
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
%%% Per-loop transformation
%%% ------------------------------------------------------------------

opt_loop({H, Body}, Order0, Doms, Blocks0, NextLabel) ->
    BodySet = ordsets:from_list(Body),
    Preds = preds(maps:keys(Blocks0), Blocks0),
    Outside = [P || P <- maps:get(H, Preds, []),
                    not ordsets:is_element(P, BodySet)],
    case Outside of
        [Pre] when Pre =/= H, is_map_key(Pre, Blocks0) ->
            {Blocks1, Pre1, Next1} = ensure_preheader(Pre, H, Blocks0, NextLabel),
            Blocks2 = licm(H, Body, Order0, Pre1, Blocks1),
            Blocks3 = strength_reduce(H, Body, loop_region(H, Order0, Doms),
                                      Blocks2),
            {Blocks3, Next1};
        _ ->
            {Blocks0, NextLabel}
    end.

%% The blocks the strength reducer is allowed to rewrite: the body of the
%% natural loop plus every block the loop header dominates.
%%
%% The natural-loop body is defined as "the blocks that can reach the back
%% edge without passing through the header", which is too narrow here: in a
%% `receive' loop the block that computes the value of a matching clause ends
%% in `ret' (or branches to the raise block), so it cannot reach the back
%% edge and is not part of any natural loop -- yet it runs once per matching
%% message and is exactly where the arithmetic of the clause lives. A block
%% dominated by the header is inside the iteration region: control only gets
%% there through the header, so it is executed at most once per iteration.
%%
%% Adding blocks is safe for the reducer, whose every rewrite is proved for
%% the operands *at the instruction being rewritten* and never moves or
%% duplicates an instruction; the set only decides how much is optimized.
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
%%% Induction-variable canonicalization and strength reduction
%%% ------------------------------------------------------------------
%%%
%%% The pass derives two facts over the SSA of a function. Both are stated
%%% for a *value* (a variable or a literal) and are only ever claimed for a
%%% value that the instruction actually produces:
%%%
%%%   integer(V)     -- whenever the use of V is reached, V is an integer
%%%   non_negative(V)-- whenever the use of V is reached, V is an integer >= 0
%%%
%%% non_negative(V) implies integer(V). The facts are computed by a monotone
%%% fixed point that starts from "unknown" everywhere (so it is sound: a fact
%%% is only derived when every rule that derives it has been established) and
%%% that treats each recognised induction variable as its own case.
%%%
%%% The only rules are:
%%%
%%%   * an integer literal (non-negative for the stronger fact);
%%%   * a canonical induction variable (see canonical_ivs/5): the entry value
%%%     and every step are integers, so the variable is an integer on every
%%%     iteration; if additionally the entry value is non-negative and every
%%%     step is non-negative, it is non-negative on every iteration. This is
%%%     an induction over the iterations, not a fixed-point claim: the loop
%%%     hypothesis is the fact for the variable itself, the base case is the
%%%     entry value and the step argument is that a non-negative integer
%%%     plus a non-negative integer step stays non-negative;
%%%   * a phi whose incoming values are all integers (respectively all
%%%     non-negative integers) -- a join of a case or an if;
%%%   * `{bif,Op}' with Op in ?INT_BIFS (?NAT_BIFS) and all operands
%%%     integers (non-negative integers). An operand is classified at that
%%%     instruction by `operand_kind/4', which prefers the `arg_types' type
%%%     annotation of the instruction and falls back to this fixed point.%%%
%%% Nothing else is accepted, and a variable defined outside the loop by an
%%% instruction this analysis cannot classify is simply "unknown" -- which
%%% only loses the rewrite. The *type* annotations described above cover
%%% exactly that gap, but only at the instruction that uses the value.
%%%
%%% TRAP: a function argument, a message lifted out of the mailbox or any
%%% other value with no definition in the function is unknown to this
%%% structural analysis. It must NOT be guessed to be an integer because it
%%% often is one at run time: doing so would turn `X * 8' into `X bsl 3' and
%%% break the float and non-number cases. The type annotation of the
%%% instruction that uses the value is the only admissible source for such a
%%% value, and it is consulted directly at that instruction.
strength_reduce(H, Body, Region, Blocks0) ->
    BodySet = ordsets:from_list(Body),
    Defs = defs_map(Blocks0),
    %% The induction variables are recognised using only the facts that hold
    %% without them (an integer literal entry value, or a variable proven
    %% integer on its own); the facts their canonical description yields are
    %% then fed into the second fixed point, which the rewrites use.
    Kinds0 = value_kinds(Defs, #{}),
    Ivs = canonical_ivs(H, BodySet, Defs, Kinds0, Blocks0),
    Kinds = value_kinds(Defs, Ivs),
    lists:foldl(fun(L, B) ->
                        reduce_block(L, Kinds, B)
                end, Blocks0, Region).
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

%% Rewrite the arithmetic in one block of the loop body. Only the operation
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
%% {ok, N} when K =:= 1 bsl N and K > 0.
power_of_two(K) when K > 0, K band (K - 1) =:= 0 ->
    {ok, pow2_shift(K, 0)};
power_of_two(_K) ->
    no.

pow2_shift(1, N) -> N;
pow2_shift(K, N) -> pow2_shift(K bsr 1, N + 1).
