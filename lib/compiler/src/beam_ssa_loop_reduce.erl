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
-module(beam_ssa_loop_reduce).
-moduledoc false.

%%% Induction-variable canonicalisation, the integrality/non-negativity proof
%%% they and the strength reducer rest on, and the whole-function strength
%%% reduction, for the opt-in loop passes (`beam_ssa_loop_opt' driver).
%%%
%%% The strength reducer rewrites an arithmetic instruction in place (same
%%% block, same destination; no instruction is ever moved or duplicated) into
%%% an exactly equivalent cheaper one, and only when the rewrite is exact for
%%% every value its operands can have:
%%%
%%% * `X * 2' becomes `X + X' for every number. Doubling by addition is exact
%%%   (multiplication by 2 and adding a float to itself both double the
%%%   exponent without rounding, subnormals included) and raises on exactly the
%%%   same values as `*', so it needs no integrality proof at all.
%%% * `X * 2^n' (n >= 1, `2^n' a literal) becomes `X bsl n' only when X is
%%%   provably an integer of BOUNDED magnitude and n <= ?MAX_SHIFT. For
%%%   integers `bsl' is exactly multiplication by a power of two, but it is
%%%   *not* exception-equivalent to `*': `bsl' raises `system_limit' for a
%%%   result past the bignum arity bound where `*' would still have returned a
%%%   bignum. A bounded operand (|X| < 2^128, the widest `#t_integer{}' range
%%%   the analysis represents) and a count <= ?MAX_SHIFT keep the result far
%%%   below that bound, so no exception is introduced.
%%% * `X div 2^n' (n >= 1) becomes `X bsr n' when X is provably a
%%%   NON-NEGATIVE integer (`div' truncates toward zero, `bsr' floors).
%%% * `X rem 2^n' (n >= 1) becomes `X band (2^n - 1)' when X is provably a
%%%   non-negative integer (`rem' of a negative dividend would not agree).
%%%
%%% Everything else is left alone: a factor or divisor that is not a literal
%%% power of two, `X div 0' (which must keep raising), `rem' by a power of two
%%% with a possibly negative dividend, and every operation with an operand
%%% whose integrality/non-negativity cannot be proved. The reducer is applied
%%% to the WHOLE FUNCTION -- every rewrite is local and proved at the
%%% instruction -- and only IV recognition is loop-scoped.
%%%
%%% Two facts are derived over the SSA of a function, both stated for a value
%%% and only ever claimed for a value that the instruction actually produces:
%%%
%%%   integer(V)      -- whenever a use of V is reached, V is an integer
%%%   non_negative(V) -- whenever a use of V is reached, V is an integer >= 0
%%%
%%% non_negative(V) implies integer(V). They are computed by a monotone fixed
%%% point that starts from "unknown" everywhere, so a fact is only derived when
%%% every rule that derives it has been established; the rules are the
%%% structural facts described in the `loop_opt' moduledoc: an integer
%%% literal; a canonical induction variable; a phi whose incoming values are
%%% all integers; a `{bif,Op}' with Op in ?INT_BIFS (?NAT_BIFS) whose operands
%%% are all integers. A fact about a value defined outside a loop holds on
%%% every path into the loop, because the definitions it rests on dominate the
%%% uses -- which is what makes the structural facts usable at a hoist point.
%%%
%%% TRAP: a function argument, a value lifted out of the mailbox or any other
%%% value with no definition in the function is unknown to this structural
%%% analysis and must NOT be guessed to be an integer because it often is one
%%% at run time; only the type annotation at the using instruction is
%%% admissible for such a value, and it is consulted directly at that
%%% instruction (`operand_kind/4').
%% The reducer is applied to EVERY block of the function (a rewrite is proved
%% at the instruction being rewritten and nothing is moved or duplicated);
%% only the induction variables, which licence `div' -> `bsr' and `rem' ->
%% `band', are loop-scoped -- an IV is defined by, and used only in blocks
%% dominated by, its header, so its fact never reaches a block outside.
-export([strength_reduce/2, defs_map/1, value_kinds/2, value_kind/2,
         kind_of_type/1]).

-include("beam_ssa.hrl").
-include("beam_types.hrl").

%% Arithmetic BIFs whose result is an integer whenever all their operands are
%% integers (the operand facts are only claimed for values that were produced,
%% so what `trunc'/`round' do with a non-number does not matter).
-define(INT_BIFS, ['+','-','*','div','rem','band','bor','bxor','bsl','bsr',
                   'abs','trunc','round']).

%% The subset of ?INT_BIFS that also preserves non-negativity: the result is
%% >= 0 whenever every operand is >= 0. Subtraction is absent (it can go
%% negative), as are float-producing operations.
-define(NAT_BIFS, ['+','*','div','rem','band','bor','bxor','bsl','bsr','abs']).

%% The integrality/non-negativity fact proven for a value, if any.
-type kind() :: 'nat' | 'int' | 'unknown'.

%% The largest literal shift count the `*' -> `bsl' rewrite may produce. The
%% emulator raises `system_limit' from `erts_bsl' when the result outgrows the
%% bignum arity bound (`BIG_ARITY_MAX', ~2 million bits), which `*' would not
%% have done; a small count together with a bounded operand (see `bounded/3')
%% keeps the result far below that bound.
-define(MAX_SHIFT, 64).

-spec strength_reduce([{beam_ssa:label(), [beam_ssa:label()]}],
                      beam_ssa:block_map()) -> beam_ssa:block_map().
strength_reduce(Loops, Blocks0) ->
    Defs = defs_map(Blocks0),
    %% The induction variables are recognised using only the facts that hold
    %% without them (an integer literal entry value, or a variable proven
    %% integer on its own); the facts their canonical description yields are
    %% then fed into the second fixed point, which the rewrites use.
    Kinds0 = value_kinds(Defs, #{}),
    Ivs = lists:foldl(fun({H, Body}, Acc) ->
                              maps:merge(Acc, canonical_ivs(H, ordsets:from_list(Body),
                                                            Defs, Kinds0, Blocks0))
                      end, #{}, Loops),
    Kinds = value_kinds(Defs, Ivs),
    lists:foldl(fun(L, B) ->
                        reduce_block(L, Kinds, B)
                end, Blocks0, maps:keys(Blocks0)).

%% The definition of every variable of the function, used to classify the
%% operands of the instructions inside the loop.
-spec defs_map(beam_ssa:block_map()) -> beam_ssa:definition_map().
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
-spec value_kinds(beam_ssa:definition_map(),
                  #{beam_ssa:b_var() => {beam_ssa:value(), integer()}}) ->
          #{beam_ssa:b_var() => kind()}.
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
    %% `operand_kinds/4' classifies each operand with the annotation on THIS
    %% instruction, which is the type `beam_ssa_type' proved for that operand
    %% at this instruction (`opt_anno_types/2' annotates an operand with its
    %% block-entry type, so the fact is valid on every path that reaches it).
    %% That is what makes a value computed from a function argument or from a
    %% value extracted out of a term -- neither of which has a defining
    %% instruction of its own -- a proven integer, and the fact is about the
    %% RESULT of the instruction, hence valid at every use of it.
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

-spec value_kind(beam_ssa:value(), #{beam_ssa:b_var() => kind()}) -> kind().
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
-spec kind_of_type(beam_types:type()) -> kind().
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

%% Rewrite the arithmetic in one block of the function. Only the operation
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
reduce_instr(#b_set{op={bif,'rem'}, args=[A, B]}=I, Kinds) ->
    case strength_rem(A, B, I, Kinds) of
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
    strength_mul_1(K, X, operand_kind(I, 1, X, Kinds), bounded(I, 1, X));
strength_mul(X, #b_literal{val=K}, I, Kinds) when is_integer(K) ->
    strength_mul_1(K, X, operand_kind(I, 0, X, Kinds), bounded(I, 0, X));
strength_mul(_A, _B, _I, _Kinds) ->
    no.

%% `X * 2' is `X + X' for EVERY number, with no proof needed: both raise
%% `badarith' on exactly the same non-numbers and both double a float exactly.
%% It is therefore preferred over the shift even when X is a proven integer,
%% because it cannot introduce a `system_limit' that `*' would not have raised.
strength_mul_1(2, X, _Kind, _Bounded) ->
    {'+', [X, X]};
%% A shift is only equivalent to `*' when X is provably INTEGRAL and of bounded
%% magnitude and the count is small. Integrality is required because `erts_bsl'
%% rounds a float first operand to an integer instead of raising, so
%% `1.5 bsl 5' succeeds (`48') where `1.5 * 32' raises `badarith'; the magnitude
%% and count bounds are required because `bsl' raises `system_limit' past the
%% bignum arity bound where `*' would have returned a bignum. See `bounded/3'
%% and ?MAX_SHIFT.
strength_mul_1(K, X, Kind, true) when K > 2 ->
    case power_of_two(K) of
        {ok, N} when N =< ?MAX_SHIFT ->
            case Kind of
                nat -> {'bsl', [X, #b_literal{val=N}]};
                int -> {'bsl', [X, #b_literal{val=N}]};
                unknown -> no
            end;
        _ ->
            no
    end;
strength_mul_1(_K, _X, _Kind, _Bounded) ->
    %% A literal factor of 0 or 1 is not a shift and `X * 0'/'X * 1' still
    %% raise for a non-number, so those are left alone. A factor that is not a
    %% power of two has no exact shift form, and a power of two whose operand
    %% magnitude is not provably bounded is left alone as well.
    no.

%% Whether the operand's magnitude is provably bounded -- i.e. below the widest
%% range the type analysis represents (2^?NUM_BITS). Only such an operand may
%% be turned into a left shift: `bsl' raises `system_limit' past the bignum
%% arity bound where `*' would have returned a bignum, so an unbounded operand
%% would turn a succeeding program into a failing one.
%%
%% Only a finite `#t_integer{}' element range proves a bound. `integer()'
%% (`elements=any') and a union do not, and NEITHER DOES A LITERAL: a literal
%% bignum operand can itself be arbitrarily large, so it is not accepted (a
%% literal-by-literal product has already been constant-folded anyway).
bounded(#b_set{anno=Anno}, Index, #b_var{}) ->
    case Anno of
        #{arg_types := #{Index := #t_integer{elements={Lo, Hi}}}} ->
            is_integer(Lo) andalso is_integer(Hi);
        _ ->
            false
    end;
bounded(_I, _Index, _Arg) ->
    false.

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

%% `X rem 2^n' for a NON-NEGATIVE dividend is `X band (2^n - 1)': for a
%% dividend >= 0 `rem' and the bit mask agree on every value (`rem' truncates
%% toward zero and `band' takes the low bits, which is the same for a
%% non-negative number), both raise `badarith' on the same non-integer
%% operands, and K >= 2 means the original cannot have been a division by
%% zero. For a dividend that may be negative the two disagree, so the rewrite
%% is skipped.
strength_rem(X, #b_literal{val=K}, I, Kinds) when is_integer(K), K > 1 ->
    case power_of_two(K) of
        {ok, _N} ->
            case operand_kind(I, 0, X, Kinds) of
                nat -> {'band', [X, #b_literal{val=K - 1}]};
                _ -> no
            end;
        no ->
            no
    end;
strength_rem(_X, _Y, _I, _Kinds) ->
    no.

%% {ok, N} when K =:= 1 bsl N and K > 0.
power_of_two(K) when K > 0, K band (K - 1) =:= 0 ->
    {ok, pow2_shift(K, 0)};
power_of_two(_K) ->
    no.

pow2_shift(1, N) -> N;
pow2_shift(K, N) -> pow2_shift(K bsr 1, N + 1).
