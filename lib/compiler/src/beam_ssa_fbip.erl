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
%% This pass implements an opt-in "FBIP" (Functional-But-In-Place)
%% transformation which enables in-place `update_record` for values
%% that are provably freshly allocated, uniquely owned and used exactly
%% once, even when the value is the result of a local function call.
%%
%% This covers a case that the regular destructive update pass
%% (beam_ssa_destructive_update) misses: that pass relies on the alias
%% analysis having proven the source variable unique, but the alias
%% analysis is conservative for values crossing a local function
%% boundary. For a local "smart constructor" such as
%%
%%     h1(X) -> R0 = mk(X), R1 = R0#r{b=1}, R2 = R1#r{c=1}, R2.
%%     mk(X) -> #r{a=X}.
%%
%% the value returned by mk/1 is a freshly allocated tuple that is
%% uniquely owned by h1/1 and used exactly once, yet the compiler still
%% emits a copying `update_record` for it.
%%
%% The pass is only run when the `fbip_opt` option is given; the gating
%% is done by beam_ssa_destructive_update:opt/2, which invokes this pass
%% after the regular alias/destructive-update transforms.
%%
%% == Algorithm ==
%%
%% 1. The set of "fresh returning" local functions is computed as a
%%    least fixpoint (starting from the empty set, growing monotonically
%%    until stable, so mutual and self recursion is handled
%%    conservatively). A function G is fresh returning if it has at least
%%    one #b_ret{} and *every* returned value is fresh (see below).
%%    The return of the pseudo block `?EXCEPTION_BLOCK` is ignored: it
%%    models "the preceding instruction failed and raised" and never
%%    returns a value to the caller.
%%
%% 2. The module is traversed repeatedly (up to ?MAX_ITERATIONS times,
%%    so that chains of updates propagate) and every copying
%%    `update_record` whose source is fresh is rewritten to use the
%%    `inplace` hint.
%%
%% A value A is considered fresh if:
%%
%%   * A is a #b_var{} (never a #b_literal{}, as literals are shared),
%%   * A is not one of the function's arguments,
%%   * A has exactly one use in the function (its defining instruction is
%%     not counted as a use, and neither is a `{succeeded,_}` guard
%%     instruction, which only reflects the success of the instruction
%%     producing A), and
%%   * A is defined by a single instruction which is one of:
%%       - `put_tuple`/`put_tuple2` (a freshly allocated heap tuple),
%%       - `bs_init_writable` or an allocating `bs_create_bin`,
%%       - a local `call` to a fresh returning function,
%%       - an `update_record` already carrying the `inplace` hint (an
%%         in-place update returns the same now-private object), or
%%       - a `phi` all of whose incoming values are themselves fresh.
%%
%% In addition the position of the definition of the source relative to the
%% update must satisfy the *position rule*. Let D be the block that defines
%% the source and U the block containing the update. The transform is
%% allowed iff
%%
%%   * D == U (the definition and the update are in the same block), or
%%   * both of the following:
%%       (1) **D dominates U** - every path from a CFG root to U passes
%%           through D, where the roots are the function's entry block and
%%           every block without predecessors (see roots/3), and
%%       (2) **every cycle through U also passes through D** -
%%           equivalently, in the CFG with D removed, U is not reachable
%%           from any successor of U.
%%
%% (1) guarantees that the value read by the update really is the object
%% allocated at D. (2) guarantees that each execution of U is preceded by
%% the execution of D that allocated the object it writes, i.e. that U is
%% executed at most once per allocation of the source. Condition (2) is
%% *not* implied by (1): a value defined before a loop but updated inside
%% the loop would be mutated once per iteration, corrupting the values that
%% earlier iterations already handed out. In ordinary Erlang source this is
%% hard to reach - a loop can only refer to a value defined before it by
%% threading that value through the loop, which is a second use and is
%% therefore already rejected by the single-use rule - so (2) is a
%% defensive guard. It is implemented anyway; the peek/skip loop of a
%% `receive' is the only cycle the SSA contains (see
%% beam_ssa:is_loop_header/1) and its blocks are generated by the compiler,
%% so no Erlang source can put an `update_record' on a cycle and there is no
%% source-level witness for the guard. The dominance half of the rule is
%% exercised by the probes in beam_ssa_fbip_SUITE, including one whose CFG
%% does contain such a cycle.
%%
%% The rule strictly generalizes the narrower "same block, or unique
%% predecessor block" rule it replaces: D == U satisfies both conditions
%% trivially, and when D is the unique predecessor of U then every path
%% into U arrives from D (so D dominates U) and every cycle through U
%% contains D. It is what lets the pass handle a value that is defined
%% before, and updated after, an intervening branch/join:
%%
%%     join(X, C) ->
%%         R0 = mk(X),                    % D (freshness rule)
%%         Y = case C of true -> 1; false -> 2 end,
%%         R0#r{b=Y}.                     % U, whose unique predecessor is
%%                                        % the join block, not D
%%
%% == Soundness ==
%%
%% The source of a rewritten update is a heap term that was allocated by
%% the very instruction defining the variable (a `put_tuple`, a writable
%% binary, or a call to a fresh returning function). The single-use rule
%% proves that no other reference to that term exists: it is not stored
%% in another term, not sent, not captured by a fun, not passed as a
%% call argument, not fed to a phi and not otherwise observable. Given the
%% single-use rule, SSA guarantees that the update reads the object
%% allocated at D, and the position rule proves that no other execution of
%% U can run between the execution of D and the update. That object is
%% therefore still private (reference count 1) when it is written, so
%% writing it in place is indistinguishable from building a copy. When
%% freshness or the position rule cannot be proven the hint is left
%% untouched and the code falls back to copying, so the transform never
%% weakens a `copy`/`reuse` that was required for safety.
%%
%% == Known limitation: lists and maps ==
%%
%% In-place reuse of *lists* and *maps* is not implemented, and it cannot
%% be implemented as a compiler-only change: no list- or map-update
%% instruction carries a `copy`/`reuse`/`inplace` hint, and the emulator
%% always allocates a fresh map for an update. `update_record` is the only
%% update instruction that can write in place, so it is the only one this
%% pass ever rewrites.

-module(beam_ssa_fbip).
-moduledoc false.

-export([opt/2, eligible/1]).

-import(lists, [all/2, member/2]).

-include("beam_ssa.hrl").

-import_record(beam_ssa_opt, [opt_st]).

-type func_info_db() :: beam_ssa_opt:func_info_db().
-type st_map() :: beam_ssa_opt:st_map().

%% Maximum number of module-wide iterations used to propagate the
%% rewritten updates through chains of updates.
-define(MAX_ITERATIONS, 16).

%% Per-function analysis context. `defs` maps a variable to its defining
%% instruction (or `ambiguous` if it is defined more than once), `uses`
%% maps a variable to the number of times it is used, `blocks` maps a
%% variable to the label of the block containing its definition (or
%% `ambiguous`), `succs` maps a block label to the list of its successor
%% blocks, `roots` is the root set of the dominance query (see roots/3): the
%% function's entry block plus every block without predecessors - which
%% additionally and harmlessly covers any unreachable block as well as the
%% pseudo block `?EXCEPTION_BLOCK`, whose terminator has no successors -
%% `args` is the list of the function's arguments and `fresh` is the set of
%% local functions known to return a fresh value.
-record #fbip_ctx{
   defs = #{} :: #{beam_ssa:var_name() => beam_ssa:b_set() | ambiguous},
   uses = #{} :: #{beam_ssa:var_name() => pos_integer()},
   blocks = #{} :: #{beam_ssa:var_name() => beam_ssa:label() | ambiguous},
   succs = #{} :: #{beam_ssa:label() => [beam_ssa:label()]},
   roots = [] :: [beam_ssa:label()],
   args = [] :: [beam_ssa:b_var()],
   fresh = #{} :: #{beam_ssa:b_local() => true}
}.
-spec opt(st_map(), func_info_db()) -> {st_map(), func_info_db()}.
opt(StMap, FuncDb) ->
    StMap1 = iterate(StMap, ?MAX_ITERATIONS),
    {StMap1, FuncDb}.

%% Eligibility precondition used by the opt-in per-pass budget/watchdog
%% driver (`beam_ssa_opt_budget', wired in
%% `beam_ssa_destructive_update:opt/2'): true when at least one function in
%% the module contains an `update_record' that is not already `inplace',
%% i.e. an update that the pass could possibly rewrite.
%%
%% This is only a NECESSARY condition - a cheap filter, O(number of
%% instructions), that lets the driver skip the pass for modules without any
%% record update at all. Whether an update is actually sound to rewrite is
%% decided by the pass itself.
-spec eligible(st_map()) -> boolean().
eligible(StMap) ->
    lists:any(fun(#opt_st{ssa=Linear}) -> eligible_linear(Linear) end,
              maps:values(StMap)).

eligible_linear([{_Lbl,#b_blk{is=Is}}|Blocks]) ->
    case lists:any(fun(#b_set{op=update_record,args=[#b_literal{val=Hint}|_]}) ->
                           Hint =/= inplace;
                      (_) ->
                           false
                   end, Is) of
        true -> true;
        false -> eligible_linear(Blocks)
    end;
eligible_linear([]) ->
    false.

%%
%% Fixpoint over the module: recompute the fresh returning functions
%% (which may have grown once new in-place updates have been introduced)
%% and rewrite another round until nothing changes.
%%
iterate(StMap, N) when N > 0 ->
    Fresh = fresh_funs(StMap),
    StMap1 = rewrite(StMap, Fresh),
    case StMap1 =:= StMap of
        true -> StMap;
        false -> iterate(StMap1, N - 1)
    end;
iterate(StMap, _N) ->
    StMap.

%%
%% Compute the set of fresh returning local functions as a least
%% fixpoint.
%%
fresh_funs(StMap) ->
    fresh_funs_1(StMap, #{}).

fresh_funs_1(StMap, Fresh0) ->
    Fresh =
        maps:fold(fun(F, #opt_st{ssa=Linear,args=Args}, Acc) ->
                          Ctx = make_ctx(Linear, Args, Fresh0),
                          case is_fresh_returning(Linear, Ctx) of
                              true -> Acc#{F => true};
                              false -> Acc
                          end
                  end, #{}, StMap),
    case Fresh =:= Fresh0 of
        true -> Fresh;
        false -> fresh_funs_1(StMap, Fresh)
    end.

%%
%% A function is fresh returning if it has at least one #b_ret{} and
%% every returned value is fresh.
%%
is_fresh_returning(Linear, Ctx) ->
    case rets(Linear) of
        [] ->
            false;
        Rets ->
            all(fun(Arg) -> is_fresh(Ctx, Arg, sets:new()) end, Rets)
    end.

%% Collect the values returned by the real returns of a function.
%%
%% The pseudo block `?EXCEPTION_BLOCK` (beam_ssa.hrl) is ignored: it models
%% "the preceding instruction failed and raised", the code generator turns
%% every branch into it into a direct raise (`{f,0}`), and it therefore never
%% returns a value to the caller. Every function whose body contains an
%% instruction that can fail (a BIF call such as `bxor/2`, a `setelement/3`
%% coming from a record update, ...) has such a block, so counting its
%% `#b_ret{}` would disqualify essentially every function from being
%% recognized as fresh returning.
rets(Linear) ->
    rets(Linear, []).

rets([{?EXCEPTION_BLOCK,_}|Rest], Acc) ->
    rets(Rest, Acc);
rets([{_,#b_blk{last=#b_ret{arg=Arg}}}|Rest], Acc) ->
    rets(Rest, [Arg|Acc]);
rets([_|Rest], Acc) ->
    rets(Rest, Acc);
rets([], Acc) ->
    Acc.

%%
%% Rewrite every copying update_record whose source is provably fresh
%% and which is positioned so that it is executed at most once per
%% allocation of the source.
%%
rewrite(StMap, Fresh) ->
    maps:map(fun(_F, #opt_st{ssa=Linear,args=Args}=St) ->
                     Ctx = make_ctx(Linear, Args, Fresh),
                     St#opt_st{ssa=rewrite_linear(Linear, Ctx)}
             end, StMap).

rewrite_linear([{Lbl,#b_blk{is=Is0}=Blk0}|Rest], Ctx) ->
    Is = rewrite_is(Is0, Lbl, Ctx),
    Blk = Blk0#b_blk{is=Is},
    [{Lbl,Blk}|rewrite_linear(Rest, Ctx)];
rewrite_linear([], _Ctx) ->
    [].

rewrite_is([#b_set{op=update_record,
                   args=[#b_literal{val=Hint},Size,Src|Updates]}=I0|Is],
           Lbl, Ctx) ->
    I = case Hint =/= inplace andalso
             is_fresh(Ctx, Src, sets:new()) andalso
             sound_position(Ctx, Lbl, Src) of
            true ->
                I0#b_set{args=[#b_literal{val=inplace},Size,Src|Updates]};
            false ->
                I0
        end,
    [I|rewrite_is(Is, Lbl, Ctx)];
rewrite_is([I|Is], Lbl, Ctx) ->
    [I|rewrite_is(Is, Lbl, Ctx)];
rewrite_is([], _Lbl, _Ctx) ->
    [].

%%
%% Freshness test.
%%
is_fresh(Ctx, #b_var{}=V, Visiting) ->
    case sets:is_element(V, Visiting) of
        true ->
            false;
        false ->
            is_fresh_var(Ctx, V, sets:add_element(V, Visiting))
    end;
is_fresh(_Ctx, _Value, _Visiting) ->
    %% Literals and function references are never fresh.
    false.

is_fresh_var(Ctx, V, Visiting) ->
    not member(V, Ctx#fbip_ctx.args) andalso
        maps:get(V, Ctx#fbip_ctx.uses, 0) =:= 1 andalso
        case Ctx#fbip_ctx.defs of
            #{ V := #b_set{}=Def } -> is_fresh_def(Ctx, Def, Visiting);
            _ -> false
        end.

is_fresh_def(Ctx, #b_set{op=Op,args=Args}, Visiting) ->
    case Op of
        put_tuple ->
            true;
        put_tuple2 ->
            true;
        bs_init_writable ->
            true;
        bs_create_bin ->
            %% A binary constructed in one go allocates a fresh binary.
            case Args of
                [#b_literal{val=alloc}|_] -> true;
                _ -> false
            end;
        call ->
            case Args of
                [#b_local{}=Callee|_] ->
                    maps:is_key(Callee, Ctx#fbip_ctx.fresh);
                _ ->
                    false
            end;
        update_record ->
            case Args of
                [#b_literal{val=inplace}|_] -> true;
                _ -> false
            end;
        phi ->
            case Args of
                [] -> false;
                _ -> all(fun({V,_Lbl}) -> is_fresh(Ctx, V, Visiting) end, Args)
            end;
        _ ->
            false
    end.

%%
%% Position rule: verify that the update can be executed at most once per
%% allocation of the source. Either the source is defined in the same block
%% as the update, or both of the following hold:
%%
%%   1. the definition block D dominates the update block U, so the object
%%      the update reads is the one allocated at D, and
%%   2. no cycle through U avoids D, so U can run at most once per
%%      execution of D.
%%
sound_position(Ctx, UpdLbl, #b_var{}=V) ->
    case maps:get(V, Ctx#fbip_ctx.blocks, undefined) of
        undefined ->
            false;
        ambiguous ->
            false;
        DefLbl when DefLbl =:= UpdLbl ->
            true;
        DefLbl ->
            dominates(Ctx, DefLbl, UpdLbl) andalso
                not cycles_through(Ctx, DefLbl, UpdLbl)
    end;
sound_position(_Ctx, _UpdLbl, _Value) ->
    false.

%%
%% Condition (1): D dominates U iff every path from a CFG root to U passes
%% through D. Equivalently: with D removed from the CFG, U is not reachable
%% from any root. The root set is built by roots/3: the function's entry
%% block plus every predecessor-less block. Using every predecessor-less
%% block as a root is conservative (a block that is only reachable through D
%% would be a harmless extra root) and keeps the check independent of the
%% order in which the blocks happen to be numbered; using the entry block as
%% well is what keeps the query meaningful if the entry block itself is on a
%% cycle, and it can only make the query stricter (see roots/3).
%%
dominates(Ctx, D, U) ->
    not reachable(Ctx, D, Ctx#fbip_ctx.roots, U).

%%
%% Condition (2): does some cycle through U avoid D? Equivalently: with D
%% removed from the CFG, is U reachable from one of U's own successors?
%%
cycles_through(Ctx, D, U) ->
    reachable(Ctx, D, maps:get(U, Ctx#fbip_ctx.succs, []), U).

%%
%% Is `Target' reachable from any block in `From' in the CFG where the
%% block `Removed' (and thus its outgoing edges) has been deleted?
%%
reachable(Ctx, Removed, From, Target) ->
    reachable_1(From, Removed, Target, sets:new(), Ctx).

reachable_1([L|Ls], Removed, Target, Seen0, Ctx) ->
    if
        L =:= Removed ->
            %% Do not traverse the removed block (nor consider it as the
            %% target: the two are never equal, D =/= U is guaranteed).
            reachable_1(Ls, Removed, Target, Seen0, Ctx);
        L =:= Target ->
            true;
        true ->
            case sets:is_element(L, Seen0) of
                true ->
                    reachable_1(Ls, Removed, Target, Seen0, Ctx);
                false ->
                    Seen = sets:add_element(L, Seen0),
                    Succs = maps:get(L, Ctx#fbip_ctx.succs, []),
                    reachable_1(Succs ++ Ls, Removed, Target, Seen, Ctx)
            end
    end;
reachable_1([], _Removed, _Target, _Seen, _Ctx) ->
    false.

%%
%% Construct the analysis context by scanning a linearized function.
%%
make_ctx(Linear, Args, Fresh) ->
    {Defs,Uses,Blocks} = scan(Linear),
    Succs = succs(Linear),
    Preds = preds(Linear),
    Roots = roots(Linear, Succs, Preds),
    #fbip_ctx{defs=Defs,uses=Uses,blocks=Blocks,succs=Succs,
              roots=Roots,args=Args,fresh=Fresh}.

%% The root set of the dominance query behind condition (1) (see
%% dominates/3): the function's entry block *plus* every block without
%% predecessors.
%%
%% The entry block is the first block of the linearized function:
%% beam_ssa:linearize/1 and beam_ssa:linearize_only/1 both start their
%% traversal at the entry block 0, so the first element of `Linear' is the
%% entry.
%%
%% The predecessor-less blocks alone are only a correct root set as long as
%% the entry block really has no predecessor. That holds for the current SSA
%% - every function gets its own entry block, and the compiler-generated
%% peek/skip loop of a `receive' (the only construct that creates an
%% intra-function back edge here, see beam_ssa:is_loop_header/1) is entered
%% by a jump from the entry block and has at least two predecessors, so it
%% is never merged into the entry, not even when the function body *starts*
%% with a `receive' - but nothing in the CFG guarantees it: a block on a
%% cycle has a predecessor, so if the entry block were a loop header the
%% root set would degenerate to the pseudo block `?EXCEPTION_BLOCK' (whose
%% terminator has no successors) or to nothing, reachable/4 would never
%% reach the update block, and condition (1) would silently become
%% vacuously true instead of being checked. Taking the entry from the
%% linearized block list makes condition (1) genuinely enforced whenever the
%% block list is non-empty.
%%
%% Adding the entry is safe by construction, it cannot make the transform
%% unsound: reachable/4 grows monotonically with its root set (it is a plain
%% graph search from the given roots), so a larger root set can only make
%% dominates/3 return `false' more often - i.e. reject more rewrites. Any
%% real path from the entry block to the update block that avoids the
%% definition block is a fortiori a path in the same, over-approximated (see
%% term_targets/1) graph, so with the entry in the root set such a path can
%% no longer be missed.
%%
roots([{Entry,_}|_], Succs, Preds) ->
    Rest = [Lbl || Lbl <- maps:keys(Succs), not is_map_key(Lbl, Preds)],
    [Entry|lists:delete(Entry, Rest)];
roots([], _Succs, _Preds) ->
    [].

scan(Linear) ->
    scan_blks(Linear, #{}, #{}, #{}).
scan_blks([{Lbl,#b_blk{is=Is,last=Last}}|Rest], Defs0, Uses0, Blocks0) ->
    {Defs,Uses,Blocks} = scan_is(Is, Lbl, Defs0, Uses0, Blocks0),
    Uses1 = count_uses(term_uses(Last), Uses),
    scan_blks(Rest, Defs, Uses1, Blocks);
scan_blks([], Defs, Uses, Blocks) ->
    {Defs,Uses,Blocks}.

scan_is([#b_set{dst=Dst}=I|Is], Lbl, Defs0, Uses0, Blocks0) ->
    Uses = case I of
               #b_set{op={succeeded,_}} ->
                   %% A {succeeded,_} guard only reflects the outcome of
                   %% the instruction producing its argument; it does not
                   %% create a reference to the value.
                   Uses0;
               _ ->
                   count_uses(set_uses(I), Uses0)
           end,
    case Dst of
        #b_var{} ->
            case Defs0 of
                #{ Dst := _ } ->
                    %% Defined more than once; treat as unsafe.
                    scan_is(Is, Lbl, Defs0#{Dst := ambiguous},
                            Uses, Blocks0#{Dst := ambiguous});
                _ ->
                    scan_is(Is, Lbl, Defs0#{Dst => I}, Uses,
                            Blocks0#{Dst => Lbl})
            end;
        none ->
            scan_is(Is, Lbl, Defs0, Uses, Blocks0)
    end;
scan_is([], _Lbl, Defs, Uses, Blocks) ->
    {Defs,Uses,Blocks}.

%% Compute the predecessor blocks of every block.
preds(Linear) ->
    lists:foldl(fun({Lbl,#b_blk{last=Last}}, Acc0) ->
                        lists:foldl(fun(Succ, Acc) ->
                                            maps:update_with(
                                              Succ, fun(Ps) -> [Lbl|Ps] end,
                                              [Lbl], Acc)
                                    end, Acc0, term_targets(Last))
                end, #{}, Linear).

%% Compute the successor blocks of every block. The dual of preds/1, and
%% built from the same over-approximating term_targets/1 so that both views
%% of the CFG agree (see the comment on term_targets/1).
succs(Linear) ->
    maps:from_list([{Lbl, term_targets(Last)} ||
                       {Lbl,#b_blk{last=Last}} <- Linear]).

%% The successor labels of a block terminator.
%%
%% This is a deliberate over-approximation: a `#b_br{}' whose condition is a
%% literal contributes both labels even though only one of the two edges is
%% live. That is conservative for both CFG queries this pass makes: extra
%% predecessors make condition (1) (dominance) harder to prove, and extra
%% successors make condition (2) (a cycle avoiding the definition block)
%% easier to find. Neither query can therefore become unsoundly optimistic.
%% A successor that is not part of the linearized function simply has no
%% outgoing edges (see succs/1), so it cannot make a query succeed either.
term_targets(#b_ret{}) ->
    [];
term_targets(#b_br{succ=Succ,fail=Fail}) ->
    [Succ,Fail];
term_targets(#b_switch{fail=Fail,list=List}) ->
    [Fail|[Lbl || {_,Lbl} <- List]].

%% Collect the variables used by an instruction.
set_uses(#b_set{args=Args}) ->
    args_uses(Args).

args_uses([Arg|Args]) ->
    value_uses(Arg) ++ args_uses(Args);
args_uses([]) ->
    [].

value_uses(#b_var{}=V) ->
    [V];
value_uses(#b_remote{mod=Mod,name=Name}) ->
    value_uses(Mod) ++ value_uses(Name);
value_uses({Val,_Lbl}) ->
    value_uses(Val);
value_uses(_Other) ->
    [].

%% Collect the variables used by a terminator.
term_uses(#b_ret{arg=Arg}) ->
    value_uses(Arg);
term_uses(#b_br{bool=Bool}) ->
    value_uses(Bool);
term_uses(#b_switch{arg=Arg}) ->
    value_uses(Arg).

count_uses([V|Vs], Uses) ->
    count_uses(Vs, maps:update_with(V, fun(N) -> N + 1 end, 1, Uses));
count_uses([], Uses) ->
    Uses.
