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
-module(beam_ssa_loop_arith).
-moduledoc false.

%%% The arithmetic LICM of the opt-in `loop_arith_licm' pass (a separate pass
%%% with its own kill switch `no_ssa_opt_loop_arith_licm', driven from
%%% `beam_ssa_loop_opt:opt_arith/1').
%%%
%%% It hoists loop-invariant arithmetic out of a loop into its preheader.
%%% `+'/'-'/'*' raise `badarith' for a non-number operand, `div'/`rem' for a
%%% zero divisor too, `band'/`bor'/`bxor' for a non-integer, and `bsl'/`bsr'
%%% for a non-integer, a negative count or a `system_limit' overflow, so
%%% hoisting one needs a proof that it cannot raise AT THE PREHEADER.
%%%
%%% A structural fact (see `beam_ssa_loop_reduce') is valid there: it rests on
%%% definitions that dominate the preheader. A type annotation is NOT: the
%%% type analysis refines a variable along the edges it flows down, so a
%%% variable that the loop narrows with a test or a comparison can be an
%%% integer at that use but need not be one at the preheader. An annotation is
%%% therefore trusted only for an operand that the loop does not narrow
%%% (`narrowed_vars/3').
-export([arith_licm/4]).

-include("beam_ssa.hrl").

%% The subset of the integer BIFs that this pass may hoist out of a loop.
%% Unlike the tag tests they can raise `badarith', so the pass has to prove
%% them non-raising first; see `arith_hoistable/6'.
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

%% Maximum number of instructions hoisted per loop. Bounds the growth of the
%% preheader on pathological input. The base LICM in `beam_ssa_loop_opt' uses
%% the same bound.
-define(MAX_HOIST, 64).

-spec arith_licm([beam_ssa:label()], [beam_ssa:label()], beam_ssa:label(),
                 beam_ssa:block_map()) -> beam_ssa:block_map().
arith_licm(Region, Order0, Pre, Blocks0) ->
    RegionSet = ordsets:from_list(Region),
    InRegion = beam_ssa_loop_cfg:loop_defs(Region, Blocks0),
    Defs = beam_ssa_loop_reduce:defs_map(Blocks0),
    Kinds = beam_ssa_loop_reduce:value_kinds(Defs, #{}),
    Narrowed = narrowed_vars(Region, Blocks0, Defs),
    Marker = marker_args(Blocks0),
    Uses = use_counts(Blocks0),
    Order = [L || L <- Order0, ordsets:is_element(L, RegionSet)],
    lists:foldl(fun(L, B) ->
                        arith_hoist_block(L, Pre, InRegion, Kinds, Narrowed,
                                          Marker, Uses, B)
                end, Blocks0, Order).

%% Variables consumed by a `{succeeded,_}' receive marker. The marker has to
%% stay in the same block, immediately after the instruction whose result it
%% guards (`beam_ssa_codegen:cg_block/3' matches the pair), so an instruction
%% that produces such a variable can only be moved when the marker over it is
%% dropped at the same time; see `droppable_markers/2'.
marker_args(Blocks) ->
    maps:fold(
      fun(_L, #b_blk{is=Is}, Acc) ->
              lists:foldl(
                fun(#b_set{op={succeeded, _}, args=[#b_var{}=V]}, A) ->
                        ordsets:add_element(V, A);
                   (_I, A) ->
                        A
                end, Acc, Is)
      end, ordsets:new(), Blocks).

%% The `{succeeded,_}' markers of the block that may be dropped along with a
%% hoist, keyed by the variable they guard (the result bool they define is the
%% value). A marker may be dropped when the checked operation is proven
%% non-raising -- which is exactly the condition under which the hoist is
%% performed -- and when its result bool is used nowhere but as the block's
%% branch condition, so that dropping it can replace that condition by the
%% constant `true'. This is the same reduction `beam_ssa_type:simplify/6'
%% performs for an operation it can prove always succeeds, and it is what lets
%% a marker-guarded instruction be hoisted without leaving the marker behind
%% in a block whose code generation it would then corrupt.
droppable_markers(#b_blk{is=Is, last=Last}, Uses) ->
    lists:foldl(
      fun(#b_set{op={succeeded, _}, args=[#b_var{}=V], dst=#b_var{}=B}, Acc) ->
              case droppable_bool(B, Last, Uses) of
                  true -> Acc#{V => B};
                  false -> Acc
              end;
         (_I, Acc) ->
              Acc
      end, #{}, Is).

droppable_bool(B, #b_br{bool=#b_var{}=B}, Uses) ->
    maps:get(B, Uses, 0) =:= 1;
droppable_bool(_B, _Last, _Uses) ->
    false.

arith_hoist_block(L, Pre, InRegion, Kinds, Narrowed, Marker, Uses, Blocks0) ->
    Blk0 = maps:get(L, Blocks0),
    Drop = droppable_markers(Blk0, Uses),
    {Keep0, Hoist} =
        arith_split(Blk0#b_blk.is, InRegion, Kinds, Narrowed, Marker, Drop,
                    0, [], []),
    case Hoist of
        [] ->
            Blocks0;
        [_ | _] ->
            %% Each hoisted instruction that was guarded by a marker takes the
            %% marker (and its branch condition) with it.
            Drop1 = maps:with([V || #b_set{dst=#b_var{}=V} <- Hoist], Drop),
            Keep = drop_markers(Keep0, Drop1),
            Last = drop_marker_branch(Blk0#b_blk.last, Drop1),
            Blocks1 = Blocks0#{L := Blk0#b_blk{is=Keep, last=Last}},
            PreBlk0 = maps:get(Pre, Blocks1),
            PreBlk = PreBlk0#b_blk{is=PreBlk0#b_blk.is ++ Hoist},
            Blocks1#{Pre := PreBlk}
    end.

%% Remove the markers of the hoisted instructions, keyed by the variable they
%% guard.
drop_markers([#b_set{op={succeeded, _}, args=[#b_var{}=V]}=I | Is], Drop) ->
    case is_map_key(V, Drop) of
        true -> drop_markers(Is, Drop);
        false -> [I | drop_markers(Is, Drop)]
    end;
drop_markers([I | Is], Drop) ->
    [I | drop_markers(Is, Drop)];
drop_markers([], _Drop) ->
    [].

%% Make a `br' whose condition was a dropped marker's bool unconditional.
drop_marker_branch(#b_br{bool=#b_var{}=B}=Br, Drop) ->
    case lists:member(B, maps:values(Drop)) of
        true -> Br#b_br{bool=#b_literal{val=true}};
        false -> Br
    end;
drop_marker_branch(Last, _Drop) ->
    Last.

%% How many times each variable of the function is used. A `{succeeded,_}'
%% marker's result bool may only be dropped when it is used exactly once, as
%% the branch condition the marker was created for.
use_counts(Blocks) ->
    maps:fold(
      fun(_L, #b_blk{is=Is, last=Last}, Acc0) ->
              Acc1 = lists:foldl(fun(#b_set{args=Args}, A) ->
                                         count_uses(Args, A)
                                 end, Acc0, Is),
              count_uses(terminator_args(Last), Acc1)
      end, #{}, Blocks).

count_uses([#b_var{}=V | T], Acc) ->
    count_uses(T, Acc#{V => maps:get(V, Acc, 0) + 1});
count_uses([_ | T], Acc) ->
    count_uses(T, Acc);
count_uses([], Acc) ->
    Acc.

terminator_args(#b_br{bool=#b_var{}=B}) -> [B];
terminator_args(#b_switch{arg=#b_var{}=A}) -> [A];
terminator_args(#b_ret{arg=#b_var{}=A}) -> [A];
terminator_args(_Last) -> [].

%% The region's definition set is never shrunk: an instruction hoisted earlier
%% in this pass has a definition inside the region, and classifying it would
%% rest on annotations that the loop may narrow. Chains of hoisted arithmetic
%% are therefore left in the loop.
arith_split([], _InRegion, _Kinds, _Narrowed, _Marker, _Drop, _Cnt, Keep,
            Hoist) ->
    {lists:reverse(Keep), lists:reverse(Hoist)};
arith_split([S | Rest], InRegion, Kinds, Narrowed, Marker, Drop, Cnt, Keep,
            Hoist) ->
    case Cnt < ?MAX_HOIST andalso
        arith_hoistable(S, InRegion, Kinds, Narrowed, Marker, Drop) of
        true ->
            arith_split(Rest, InRegion, Kinds, Narrowed, Marker, Drop, Cnt + 1,
                        Keep, [S | Hoist]);
        false ->
            arith_split(Rest, InRegion, Kinds, Narrowed, Marker, Drop, Cnt,
                        [S | Keep], Hoist)
    end.

arith_hoistable(#b_set{op={bif,Op}, args=[A, B], dst=Dst}=I, InRegion, Kinds,
                Narrowed, Marker, Drop) ->
    marker_ok(Dst, Marker, Drop) andalso
        lists:member(Op, ?ARITH_BIFS) andalso
        invariant_arith_arg(A, InRegion) andalso
        invariant_arith_arg(B, InRegion) andalso
        is_integer_kind(pre_kind(I, 0, A, Narrowed, Kinds)) andalso
        is_integer_kind(pre_kind(I, 1, B, Narrowed, Kinds)) andalso
        second_operand_ok(Op, B);
arith_hoistable(_I, _InRegion, _Kinds, _Narrowed, _Marker, _Drop) ->
    false.

%% A `{succeeded,_}' marker over the result of an instruction normally pins it
%% to its block. The instruction may still be hoisted when the marker itself
%% is dropped along with it: the operation is proven non-raising, so the
%% marker is unconditionally true and the branch on it is made unconditional.
marker_ok(Dst, Marker, Drop) ->
    not ordsets:is_element(Dst, Marker) orelse is_map_key(Dst, Drop).

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
second_operand_ok(Op, B) when Op =:= 'bsl'; Op =:= 'bsr' ->
    is_shift_literal(B);
second_operand_ok(_Op, _B) ->
    true.

%% `bsl'/`bsr' accept any integer first operand but raise for a negative or a
%% non-integer count and can hit `system_limit' for a huge one, so the count
%% must be a literal in `0..?MAX_LITERAL_SHIFT'.
is_shift_literal(#b_literal{val=N}) when is_integer(N) ->
    N >= 0 andalso N =< ?MAX_LITERAL_SHIFT;
is_shift_literal(_B) ->
    false.

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
    case beam_ssa_loop_reduce:value_kind(V, Kinds) of
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
        #{arg_types := #{Index := Type}} ->
            beam_ssa_loop_reduce:kind_of_type(Type);
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
