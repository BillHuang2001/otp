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
%% In addition the definition of the source must either be in the same
%% basic block as the update or in the unique predecessor block of the
%% update's block. This guarantees that the update can never be executed
%% more than once per allocation of the source, and in particular
%% prevents an update inside a loop from mutating a value that was
%% created before the loop (which would corrupt values already handed
%% out in earlier iterations).
%%
%% == Soundness ==
%%
%% The source of a rewritten update is a heap term that was allocated by
%% the very instruction defining the variable (a `put_tuple`, a writable
%% binary, or a call to a fresh returning function). The single-use rule
%% proves that no other reference to that term exists: it is not stored
%% in another term, not sent, not captured by a fun, not passed as a
%% call argument, not fed to a phi and not otherwise observable. The
%% position rule above proves that each execution of the update sees a
%% freshly allocated object that is used only there, so its reference
%% count is 1 and writing it in place is indistinguishable from building
%% a copy. When freshness cannot be proven the hint is left untouched
%% and the code falls back to copying, so the transform never weakens a
%% `copy`/`reuse` that was required for safety.

-module(beam_ssa_fbip).
-moduledoc false.

-export([opt/2]).

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
%% `ambiguous`), `preds` maps a block label to the list of its
%% predecessor blocks, `args` is the list of the function's arguments
%% and `fresh` is the set of local functions known to return a fresh
%% value.
-record #fbip_ctx{
   defs = #{} :: #{beam_ssa:var_name() => beam_ssa:b_set() | ambiguous},
   uses = #{} :: #{beam_ssa:var_name() => pos_integer()},
   blocks = #{} :: #{beam_ssa:var_name() => beam_ssa:label() | ambiguous},
   preds = #{} :: #{beam_ssa:label() => [beam_ssa:label()]},
   args = [] :: [beam_ssa:b_var()],
   fresh = #{} :: #{beam_ssa:b_local() => true}
}.

-spec opt(st_map(), func_info_db()) -> {st_map(), func_info_db()}.
opt(StMap, FuncDb) ->
    StMap1 = iterate(StMap, ?MAX_ITERATIONS),
    {StMap1, FuncDb}.

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

rets(Linear) ->
    rets(Linear, []).

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
             sound_position(Ctx, Lbl, Src) andalso
             is_fresh(Ctx, Src, sets:new()) of
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
%% Verify that the update can be executed at most once per allocation of
%% the source: either the source is defined in the same block as the
%% update, or in the unique predecessor block of the update's block.
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
            maps:get(UpdLbl, Ctx#fbip_ctx.preds, []) =:= [DefLbl]
    end;
sound_position(_Ctx, _UpdLbl, _Value) ->
    false.

%%
%% Construct the analysis context by scanning a linearized function.
%%
make_ctx(Linear, Args, Fresh) ->
    {Defs,Uses,Blocks} = scan(Linear),
    Preds = preds(Linear),
    #fbip_ctx{defs=Defs,uses=Uses,blocks=Blocks,preds=Preds,
              args=Args,fresh=Fresh}.

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
