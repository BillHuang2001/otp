// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The **oracle** battery: one Erlang program, run by the *system* `erl` (OTP 29 /
//! erts-17.1) to produce the reference rendering of every ETS case in the
//! differential suite.
//!
//! It is embedded verbatim here and written to `$TMPDIR` at test time — the
//! suite compiles it with the system `erlc` and runs
//! `beam_ets_diff:run()` with `-noshell`. Nothing in this file is compiled by
//! Rust; it is data.
//!
//! ## Rules the program obeys (so its output is comparable at all)
//!
//! * Every case prints exactly one `name|rendered` line via [`emit/2`].
//! * Every result is reduced to the *canonical* textual form produced by [`r/1`]
//!   — small integers, atoms, tuples and proper lists only. Table ids/handles,
//!   `memory`, `pids`, floats and binaries are never rendered.
//! * `srt/1` renders a term list **sorted** (order-independent), and is used for
//!   every result whose order is not part of the contract (`set`/`bag`/
//!   `duplicate_bag` `lookup`/`tab2list`); `inorder/1` is used only where the
//!   order *is* the contract (`ordered_set` iteration).
//! * `cap/1` renders a possibly-raising call as `badarg`, `error:<reason>` or
//!   `ok:<rendered>` so an exception is compared as a value.
//! * `ets:size/1` **does not exist** in OTP 29 (`ets:info(T, size)` is the
//!   supported spelling) — see the suite's module docs.
//!
//! The Rust side (`battery.rs`) implements the identical case names and the
//! identical renderer, so the two outputs can be compared line by line.

/// The oracle battery, verbatim.
pub const ERL_PROGRAM: &str = r#"-module(beam_ets_diff).
-export([run/0]).

%% ------------------------- canonical renderer -------------------------
%% Small integers, atoms, tuples and proper lists only. This is the identical
%% renderer the Rust side uses, so the two textual forms are comparable.
r(T) when is_integer(T) -> integer_to_list(T);
r(T) when is_atom(T) -> atom_to_list(T);
r(T) when is_tuple(T) -> "{" ++ join([r(E) || E <- tuple_to_list(T)]) ++ "}";
r([]) -> "[]";
r(T) when is_list(T) -> "[" ++ join([r(E) || E <- T]) ++ "]".

join(L) -> lists:flatten(lists:join(",", L)).
%% Compose an already-rendered tuple / list.
t(L) -> "{" ++ join(L) ++ "}".
l(L) -> "[" ++ join(L) ++ "]".
%% Order-independent (sorted multiset) and in-order renderings of a term list.
srt(Ts) -> l(lists:sort([r(T) || T <- Ts])).
inorder(Ts) -> l([r(T) || T <- Ts]).

emit(Name, S) -> io:format("~s|~s~n", [Name, S]).

%% Outcome of a possibly-failing call: "ok:<rendered>" or the error class.
cap(F) ->
    try "ok:" ++ r(F())
    catch error:badarg -> "badarg";
          error:E -> "error:" ++ atom_to_list(E)
    end.

%% Outcome of a possibly-failing call whose result the caller has *already*
%% reduced to the canonical rendering — the access matrix's per-operation probes,
%% where a read whose iteration order is not part of the contract is rendered
%% sorted (`srt/1`) before it reaches `cap/1`'s shape.
cap_str(F) ->
    try "ok:" ++ F()
    catch error:badarg -> "badarg";
          error:E -> "error:" ++ atom_to_list(E)
    end.
new(Kind) -> ets:new(diff, [Kind, public]).
new(Kind, Keypos) -> ets:new(diff, [Kind, public, {keypos, Keypos}]).
tsize(T) -> ets:info(T, size).

%% ------------------------------- battery -------------------------------

run() ->
    try
        battery()
    catch Cl:Er:St ->
        io:format(standard_error, "BATTERY_CRASH ~p:~p~n~p~n", [Cl, Er, St])
    end.

battery() ->
    KindList = [{set, "set"}, {ordered_set, "ordered_set"},
                {bag, "bag"}, {duplicate_bag, "duplicate_bag"}],
    [ begin
          emit(KName ++ "_insert_lookup", kind_insert_lookup(Kind)),
          emit(KName ++ "_insert_new", kind_insert_new(Kind)),
          emit(KName ++ "_identical_object", kind_identical_object(Kind)),
          emit(KName ++ "_delete_object_same", kind_delete_object_same(Kind)),
          emit(KName ++ "_delete_object_other", kind_delete_object_other(Kind)),
          emit(KName ++ "_tab2list_count", kind_tab2list_count(Kind)),
          emit(KName ++ "_delete_by_key", kind_delete_by_key(Kind)),
          emit(KName ++ "_take", kind_take(Kind))
      end || {Kind, KName} <- KindList ],

    emit("keypos2_basic", keypos2_basic()),
    emit("keypos2_update_counter", keypos2_update_counter()),
    emit("keypos2_update_element", keypos2_update_element()),

    emit("ordered_iteration", ordered_iteration()),
    emit("ordered_absent_bounds", ordered_absent_bounds()),
    emit("ordered_empty_nav", ordered_empty_nav()),
    emit("ordered_mixed_keys", ordered_mixed_keys()),
    emit("ordered_float_key_order", ordered_float_key_order()),
    emit("ordered_replaced_key_term", ordered_replaced_key_term()),

    %% The multi-object + ordered discrimination group (`battery/multikind.rs`).
    emit("set_insert_new_key_exists", insert_new_key_exists(set)),
    emit("ordered_set_insert_new_key_exists", insert_new_key_exists(ordered_set)),
    emit("bag_insert_new_key_exists", insert_new_key_exists(bag)),
    emit("duplicate_bag_insert_new_key_exists", insert_new_key_exists(duplicate_bag)),
    emit("bag_duplicate_object_multiplicity", duplicate_object_multiplicity(bag)),
    emit("duplicate_bag_duplicate_object_multiplicity", duplicate_object_multiplicity(duplicate_bag)),
    emit("bag_delete_object_all_occurrences", delete_object_all_occurrences(bag)),
    emit("duplicate_bag_delete_object_all_occurrences", delete_object_all_occurrences(duplicate_bag)),
    emit("bag_size_counts_objects", size_counts_objects(bag)),
    emit("duplicate_bag_size_counts_objects", size_counts_objects(duplicate_bag)),
    emit("bag_take_and_delete_key", take_and_delete_key(bag)),
    emit("duplicate_bag_take_and_delete_key", take_and_delete_key(duplicate_bag)),
    emit("bag_update_counter_badarg", update_counter_badarg(bag)),
    emit("duplicate_bag_update_counter_badarg", update_counter_badarg(duplicate_bag)),
    emit("bag_update_element_badarg", update_element_badarg(bag)),
    emit("duplicate_bag_update_element_badarg", update_element_badarg(duplicate_bag)),
    emit("duplicate_bag_select_replace", duplicate_bag_select_replace()),
    emit("duplicate_bag_select_replace_identity", duplicate_bag_select_replace_identity()),
    emit("multi_lookup_element_or", multi_lookup_element_or()),
    emit("multi_lookup_element", multi_lookup_element()),
    emit("bag_match_delete", multi_match_delete(bag)),
    emit("bag_select_delete", multi_select_delete(bag)),
    emit("bag_keypos2", multi_kind_keypos2(bag)),
    emit("duplicate_bag_match_delete", multi_match_delete(duplicate_bag)),
    emit("duplicate_bag_select_delete", multi_select_delete(duplicate_bag)),
    emit("duplicate_bag_keypos2", multi_kind_keypos2(duplicate_bag)),
    emit("duplicate_bag_select_replace_changed_key", duplicate_bag_select_replace_changed_key()),
    emit("ordered_set_delete_object_exact_eq", ordered_set_delete_object_exact_eq()),
    emit("ordered_set_keypos2", ordered_set_keypos2()),
    emit("ordered_set_update_element", ordered_set_update_element()),
    emit("ordered_set_update_counter_default", ordered_set_update_counter_default()),
    emit("hash_nav_absent", hash_nav_absent()),
    emit("hash_key_walk", hash_key_walk()),
    emit("update_counter_3_variants", update_counter_3_variants()),
    emit("update_counter_3_absent", update_counter_3_absent()),
    emit("update_counter_3_threshold", update_counter_3_threshold()),
    emit("update_counter_4_default", update_counter_4_default()),
    emit("update_counter_4_list", update_counter_4_list()),
    emit("update_counter_4_threshold", update_counter_4_threshold()),
    emit("update_counter_threshold_default", update_counter_threshold_default()),
    emit("update_counter_multi_kind", update_counter_multi_kind()),
    emit("update_element_variants", update_element_variants()),

    emit("update_counter_bignum_overflow", update_counter_bignum_overflow()),
    emit("update_counter_bignum_kinds", update_counter_bignum_kinds()),
    emit("update_counter_bignum_operand", update_counter_bignum_operand()),
    emit("update_counter_bignum_element", update_counter_bignum_element()),
    emit("update_counter_bignum_add", update_counter_bignum_add()),
    emit("update_counter_bignum_mixed_sign", update_counter_bignum_mixed_sign()),
    emit("update_counter_bignum_threshold", update_counter_bignum_threshold()),
    emit("update_counter_bignum_warp_value", update_counter_bignum_warp_value()),
    emit("update_counter_bignum_list_ops", update_counter_bignum_list_ops()),
    emit("update_counter_bignum_position_badarg", update_counter_bignum_position_badarg()),
    emit("take_and_lookup_element", take_and_lookup_element()),

    emit("select_head_vars", select_head_vars()),
    emit("select_wildcard_literal", select_wildcard_literal()),
    emit("select_guard_comparison", select_guard_comparison()),
    emit("select_guard_typetest", select_guard_typetest()),
    emit("select_guard_andalso_orelse", select_guard_andalso_orelse()),
    emit("select_guard_div", select_guard_div()),
    emit("select_guard_float", select_guard_float()),
    emit("select_guard_element", select_guard_element()),
    emit("select_guard_abs", select_guard_abs()),
    emit("select_guard_map_size", select_guard_map_size()),
    emit("select_guard_is_map_key", select_guard_is_map_key()),
    emit("select_guard_map_get", select_guard_map_get()),
    emit("select_guard_tuple_size", select_guard_tuple_size()),
    emit("select_guard_size_family", select_guard_size_family()),
    emit("select_guard_float_arith", select_guard_float_arith()),
    emit("select_body_whole", select_body_whole()),
    emit("select_body_var", select_body_var()),
    emit("select_body_const", select_body_const()),
    emit("select_body_construct", select_body_construct()),
    emit("select_body_map_construct", select_body_map_construct()),
    emit("select_delete_and_count", select_delete_and_count()),
    emit("match_family", match_family()),
    emit("malformed_specs", malformed_specs()),

    emit("match_spec_run_bindings", match_spec_run_bindings()),
    emit("match_spec_run_repeated_var", match_spec_run_repeated_var()),
    emit("match_spec_run_multi_clause", match_spec_run_multi_clause()),
    emit("match_spec_run_empty_and_badarg", match_spec_run_empty_and_badarg()),
    emit("body_error_exit", body_error_exit()),
    emit("fun2ms_spec_family", fun2ms_spec_family()),
    emit("select_replace_small_int", select_replace_small_int()),
    emit("select_replace_arity1", select_replace_arity1()),
    emit("select_replace_bag_badarg", select_replace_bag_badarg()),
    emit("select_replace_changed_key_badarg", select_replace_changed_key_badarg()),
    emit("select_replace_identity", select_replace_identity()),
    emit("select_limit_chain", select_limit_chain()),
    emit("match_limit_chain", match_limit_chain()),
    emit("match_object_limit_chain", match_object_limit_chain()),
    emit("select_limit_zero_badarg", select_limit_zero_badarg()),
    emit("select_limit_empty_table", select_limit_empty_table()),

    emit("info_name_named_table", info_name_named_table()),
    emit("info_option_fields", info_option_fields()),
    emit("info_static_fields", info_static_fields()),
    emit("info_unknown_key", info_unknown_key()),
    emit("lookup_element_multi", lookup_element_multi()),
    emit("insert_list_atomic", insert_list_atomic()),
    emit("reclamation_observable", reclamation_observable()),

    emit("ownership_give_away", ownership_give_away()),
    emit("ownership_heir", ownership_heir()),
    emit("access_control", access_control()),
    emit("setopts_surface", setopts_surface()),

    emit("set_large", set_large()),
    emit("ordered_large", ordered_large()),
    ok.

%% ------------------------------ per-kind cases ------------------------------

kind_insert_lookup(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}), ets:insert(T, {3, 30}),
    R1 = ets:lookup(T, 1), R2 = ets:lookup(T, 9),
    M1 = ets:member(T, 2), M2 = ets:member(T, 9),
    S1 = tsize(T),
    ets:delete(T, 2),
    S2 = tsize(T), M3 = ets:member(T, 2),
    t([srt(R1), srt(R2), r(M1), r(M2), r(S1), r(S2), r(M3)]).

kind_insert_new(Kind) ->
    T = new(Kind),
    B1 = ets:insert_new(T, {1, 10}),
    B2 = ets:insert_new(T, {1, 20}),
    B3 = ets:insert_new(T, {2, 20}),
    t([r(B1), r(B2), r(B3), r(tsize(T)), srt(ets:lookup(T, 1))]).

kind_identical_object(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}),
    ets:insert(T, {1, 10}),
    ets:insert(T, {1, 20}),
    t([r(tsize(T)), srt(ets:lookup(T, 1))]).

kind_delete_object_same(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    ets:delete_object(T, {1, 20}),
    t([r(tsize(T)), r(ets:member(T, 1)), srt(ets:lookup(T, 1))]).

kind_delete_object_other(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    ets:delete_object(T, {1, 99}),
    t([r(tsize(T)), r(ets:member(T, 1)), srt(ets:lookup(T, 1))]).

kind_tab2list_count(Kind) ->
    T = new(Kind),
    [ets:insert(T, {K, K * 10}) || K <- [1, 2, 3, 4, 5]],
    t([r(length(ets:tab2list(T))), r(tsize(T))]).

kind_delete_by_key(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    ets:delete(T, 1), ets:delete(T, 99),
    t([r(tsize(T)), r(ets:member(T, 1)), r(ets:member(T, 2))]).

kind_take(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}),
    case Kind of
        bag -> ets:insert(T, {1, 20});
        duplicate_bag -> ets:insert(T, {1, 20});
        _ -> ok
    end,
    ets:insert(T, {2, 20}),
    Taken = ets:take(T, 1),
    Absent = ets:take(T, 99),
    t([srt(Taken), srt(Absent), r(tsize(T)), r(ets:member(T, 1))]).

%% ------------------------------ keypos cases ------------------------------

keypos2_basic() ->
    T = new(set, 2),
    ets:insert(T, {a, 1}), ets:insert(T, {b, 2}),
    L1 = ets:lookup(T, 1),
    ets:insert(T, {z, 1}),
    L2 = ets:lookup(T, 1),
    t([srt(L1), srt(L2), r(tsize(T)), r(ets:member(T, 2)), r(ets:member(T, 1))]).

keypos2_update_counter() ->
    T = new(set, 2),
    ets:insert(T, {tag, 1, 10}),
    V = ets:update_counter(T, 1, 5),
    t([r(V), srt(ets:lookup(T, 1))]).

keypos2_update_element() ->
    T = new(set, 2),
    ets:insert(T, {tag, 1, 10, 20}),
    R = ets:update_element(T, 1, {3, 99}),
    t([r(R), srt(ets:lookup(T, 1))]).

%% ------------------------------ ordered_set navigation ------------------------------

walk_next(_T, '$end_of_table', Acc) -> lists:reverse(Acc);
walk_next(T, K, Acc) -> walk_next(T, ets:next(T, K), [K | Acc]).

walk_prev(_T, '$end_of_table', Acc) -> lists:reverse(Acc);
walk_prev(T, K, Acc) -> walk_prev(T, ets:prev(T, K), [K | Acc]).

ordered_iteration() ->
    T = new(ordered_set),
    [ets:insert(T, {K, K * 10}) || K <- [3, 1, 2]],
    Fwd = walk_next(T, ets:first(T), []),
    Bwd = walk_prev(T, ets:last(T), []),
    TabKeys = [element(1, O) || O <- ets:tab2list(T)],
    t([r(ets:first(T)), r(ets:last(T)),
       inorder(Fwd), inorder(Bwd), inorder(TabKeys),
       r(ets:next(T, 3)), r(ets:prev(T, 1))]).

ordered_absent_bounds() ->
    T = new(ordered_set),
    [ets:insert(T, {K, K}) || K <- [10, 20, 30]],
    t([r(ets:next(T, 15)), r(ets:prev(T, 15)),
       r(ets:next(T, 5)), r(ets:prev(T, 5)),
       r(ets:next(T, 35)), r(ets:prev(T, 35))]).

ordered_empty_nav() ->
    T = new(ordered_set),
    t([r(ets:first(T)), r(ets:last(T)),
       r(ets:next(T, 5)), r(ets:prev(T, 5))]).

%% The one observable `ordered_set` contract: the key *order* — pinned across
%% mixed key types (integers incl. negative and zero, atoms, tuples, lists,
%% binaries). Floats and binaries are not renderable, so each inserted object
%% carries a small-integer tag as element 2 and the case renders the resulting
%% *permutation of tags* (plus the tag of the first/last object and the
%% `ets:next`/`ets:prev` chains), never a binary key.
%%
%% The order is the Erlang term order: number < atom < tuple < list < bitstring.
%% (The integer/float interleaving is pinned separately by
%% `ordered_float_key_order/0`.)
ordered_mixed_keys() ->
    In = [{5, i1}, {-3, i2}, {0, i3}, {abc, a1}, {xyz, a2},
          {{1, 2}, t1}, {{a, b}, t2}, {[1, 2], l1}, {<<1, 2>>, b1}],
    T = new(ordered_set),
    [ets:insert(T, {K, Idx}) || {K, Idx} <- In],
    Objs = ets:tab2list(T),
    IdxOrder = inorder([element(2, O) || O <- Objs]),
    First = r(element(2, hd(Objs))),
    Last = r(element(2, lists:last(Objs))),
    NextChain = inorder(walk_next_idx(T, ets:first(T), In, [])),
    PrevChain = inorder(walk_prev_idx(T, ets:last(T), In, [])),
    t([IdxOrder, First, Last, NextChain, PrevChain]).

%% Erlang numbers order by *value* across representations: `-3 < -2.5 < 0 <
%% 1.5 < 5`, so an `ordered_set` interleaves integers and floats. This is the
%% arithmetic key order (`erl_db_tree.c` cmp_key_eq / CMP = erts_cmp(a,b,0,0)),
%% which `beam_ets`'s `compare::cmp_arith` reproduces, so the case agrees.
ordered_float_key_order() ->
    In = [{5, i1}, {-3, i2}, {0, i3}, {1.5, f1}, {-2.5, f2}],
    T = new(ordered_set),
    [ets:insert(T, {K, Idx}) || {K, Idx} <- In],
    Objs = ets:tab2list(T),
    IdxOrder = inorder([element(2, O) || O <- Objs]),
    First = r(element(2, hd(Objs))),
    Last = r(element(2, lists:last(Objs))),
    NextChain = inorder(walk_next_idx(T, ets:first(T), In, [])),
    PrevChain = inorder(walk_prev_idx(T, ets:last(T), In, [])),
    t([IdxOrder, First, Last, NextChain, PrevChain]).
%% An insert whose key is ARITHMETICALLY EQUAL to a stored key but a DIFFERENT
%% term (`{1,a}` then `{1.0,b}`) replaces the object *and* the stored key term,
%% so `ets:first`/`ets:last`/`ets:next` report the last-inserted key term. Floats
%% are not renderable, so the case renders `is_float/1` of the key terms (an atom)
%% instead of the terms themselves.
ordered_replaced_key_term() ->
    IntThenFloat = replaced_key_terms(1, 1.0),
    FloatThenInt = replaced_key_terms(1.0, 1),
    Three = begin
                T = new(ordered_set),
                ets:insert(T, {1, a}),
                ets:insert(T, {1.0, b}),
                ets:insert(T, {2, c}),
                t([r(tsize(T)), r(is_float(ets:first(T))),
                   r(is_float(ets:last(T))), r(ets:next(T, ets:first(T)))])
            end,
    t([IntThenFloat, FloatThenInt, Three]).

%% The 2-insert collapse: `{K1, a}` then `{K2, b}`, rendered as
%% `{Size, is_float(First), is_float(Last), Next}`.
replaced_key_terms(K1, K2) ->
    T = new(ordered_set),
    ets:insert(T, {K1, a}),
    ets:insert(T, {K2, b}),
    t([r(tsize(T)), r(is_float(ets:first(T))), r(is_float(ets:last(T))),
       r(ets:next(T, ets:first(T)))]).
%% The `ets:next`/`ets:prev` chain, rendering the tag of each visited key (the
%% key -> tag lookup is `lists:keyfind/3` against the insert list).
walk_next_idx(_T, '$end_of_table', _In, Acc) -> lists:reverse(Acc);
walk_next_idx(T, K, In, Acc) ->
    {K, Idx} = lists:keyfind(K, 1, In),
    walk_next_idx(T, ets:next(T, K), In, [Idx | Acc]).

walk_prev_idx(_T, '$end_of_table', _In, Acc) -> lists:reverse(Acc);
walk_prev_idx(T, K, In, Acc) ->
    {K, Idx} = lists:keyfind(K, 1, In),
    walk_prev_idx(T, ets:prev(T, K), In, [Idx | Acc]).

%% ------------------- multi-object + ordered discrimination -------------------
%% (the Rust counterpart is `battery/multikind.rs`)

%% `insert_new/2` refuses whenever the KEY exists — for every kind, including
%% `duplicate_bag`, and even for an object identical to a stored one.
insert_new_key_exists(Kind) ->
    T = new(Kind),
    B1 = ets:insert_new(T, {1, 10}),
    B2 = ets:insert_new(T, {1, 20}),
    B3 = ets:insert_new(T, {1, 10}),
    B4 = ets:insert_new(T, {2, 20}),
    t([r(B1), r(B2), r(B3), r(B4), r(tsize(T)),
       srt(ets:lookup(T, 1)), srt(ets:lookup(T, 2))]).

%% A `bag` keeps distinct objects (an exact duplicate is a no-op); a
%% `duplicate_bag` keeps every insert. The multiplicity shows through size,
%% lookup, select/match_object bodies, the match result count and select_count.
duplicate_object_multiplicity(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    t([r(tsize(T)), srt(ets:lookup(T, 1)),
       srt(ets:select(T, [{{'_', '_'}, [], ['$_']}])),
       srt(ets:match_object(T, {'_', '_'})),
       r(length(ets:match(T, {'$1', '$2'}))),
       r(ets:select_count(T, [{{'_', '_'}, [], [true]}]))]).

%% `delete_object/2` removes EVERY stored object equal to it.
delete_object_all_occurrences(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    ets:delete_object(T, {1, 10}),
    t([r(tsize(T)), srt(ets:lookup(T, 1)), srt(ets:tab2list(T))]).

%% `size` counts OBJECTS, not keys (three objects under key 1, two under key 2).
size_counts_objects(Kind) ->
    T = new(Kind),
    [ets:insert(T, {1, V}) || V <- [10, 20, 30]],
    [ets:insert(T, {2, V}) || V <- [40, 50]],
    t([r(tsize(T)), r(length(ets:tab2list(T))),
       r(length(ets:select(T, [{{'_', '_'}, [], ['$_']}]))),
       r(ets:select_count(T, [{{'_', '_'}, [], [true]}]))]).

%% `take/2` removes the whole key; a following `delete/2` empties the table.
take_and_delete_key(Kind) ->
    T = new(Kind),
    [ets:insert(T, {1, V}) || V <- [10, 20, 30]],
    ets:insert(T, {2, 40}),
    Taken = ets:take(T, 1),
    S1 = r(tsize(T)),
    ets:delete(T, 2),
    t([srt(Taken), S1, r(tsize(T)), r(ets:member(T, 1)), r(ets:member(T, 2))]).

%% `update_counter/3,4` is `badarg` on the multi kinds (only set/ordered_set have
%% counters), with and without a default object.
update_counter_badarg(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}),
    t([cap(fun() -> ets:update_counter(T, 1, 1) end),
       cap(fun() -> ets:update_counter(T, 1, 1, {1, 0}) end)]).

%% `update_element/3` is `badarg` on the multi kinds and writes nothing.
update_element_badarg(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10, 20}),
    t([cap(fun() -> ets:update_element(T, 1, {3, 99}) end), srt(ets:lookup(T, 1))]).

%% `select_replace/2` works on a `duplicate_bag` (only `bag` is refused).
duplicate_bag_select_replace() ->
    T = new(duplicate_bag),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    S = [{{'$1', '$2'}, [], [{{'$1', {'*', '$2', 2}}}]}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

%% The identity `'$_'` body counts both copies as replaced and keeps the dup.
duplicate_bag_select_replace_identity() ->
    T = new(duplicate_bag),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 10}),
    C = cap(fun() -> ets:select_replace(T, [{{'_', '_'}, [], ['$_']}]) end),
    t([C, srt(ets:tab2list(T))]).

%% `lookup_element/4` on a multi kind: a present key yields the element LIST, an
%% absent key the default, and a too-short object is `badarg`.
multi_lookup_element_or() ->
    T = new(bag),
    ets:insert(T, {1, 10, 100}), ets:insert(T, {1, 20, 200}),
    t([r(ets:lookup_element(T, 1, 2, dflt)),
       r(ets:lookup_element(T, 99, 2, dflt)),
       cap(fun() -> ets:lookup_element(T, 1, 4, dflt) end)]).

%% `ordered_set` `delete_object/2` uses the exact `=:=` object identity: an
%% inserted `{1, a}` survives a `delete_object/{1.0, a}`.
ordered_set_delete_object_exact_eq() ->
    T = new(ordered_set),
    ets:insert(T, {1, a}),
    ets:delete_object(T, {1.0, a}),
    D1 = r(tsize(T)),
    ets:delete_object(T, {1, a}),
    t([D1, r(tsize(T))]).

%% A `{keypos, 2}` `ordered_set` orders and navigates by the second element.
ordered_set_keypos2() ->
    T = new(ordered_set, 2),
    ets:insert(T, {a, 3}), ets:insert(T, {b, 1}), ets:insert(T, {c, 2}),
    t([r(tsize(T)),
       inorder([element(1, O) || O <- ets:tab2list(T)]),
       r(ets:first(T)), r(ets:last(T)),
       inorder([element(1, O) || O <- ets:lookup(T, 2)])]).

%% `ordered_set` `update_counter/3` and `update_element/3` both work.
ordered_set_update_element() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10, 20}),
    V = ets:update_counter(T, 1, 5),
    U = ets:update_element(T, 1, {3, 99}),
    t([r(V), r(U), inorder(ets:lookup(T, 1))]).

%% `ordered_set` `update_counter/4` deposits the default object for an absent key.
ordered_set_update_counter_default() ->
    T = new(ordered_set),
    V = ets:update_counter(T, 1, 5, {1, 0}),
    t([r(V), inorder(ets:lookup(T, 1))]).

%% Hash-kind navigation: the empty-table first/last are '$end_of_table' and an
%% absent anchor is `badarg` (unlike an `ordered_set`'s range bound).
hash_nav_absent() ->
    l([begin
           T = new(Kind),
           E1 = r(ets:first(T)), E2 = r(ets:last(T)),
           NF = cap(fun() -> ets:next(T, 5) end),
           PF = cap(fun() -> ets:prev(T, 5) end),
           ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
           NA = cap(fun() -> ets:next(T, 99) end),
           PA = cap(fun() -> ets:prev(T, 99) end),
           t([E1, E2, NF, PF, NA, PA])
       end || Kind <- [set, bag, duplicate_bag]]).

%% A hash-kind first+next walk visits each key once (order not contract → sorted).
hash_key_walk() ->
    l([begin
           T = new(Kind),
           ets:insert(T, {1, 10}), ets:insert(T, {1, 20}), ets:insert(T, {2, 20}),
           Keys = walk_next(T, ets:first(T), []),
           t([srt(Keys), r(length(Keys))])
       end || Kind <- [set, bag, duplicate_bag]]).

%% `lookup_element/3` on a multi kind: a present key yields the element LIST,
%% while an absent key (no default) and a too-short object are both `badarg`.
multi_lookup_element() ->
    T = new(bag),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 20}), ets:insert(T, {2, 5}),
    t([srt(ets:lookup_element(T, 1, 2)),
       cap(fun() -> ets:lookup_element(T, 99, 2) end),
       cap(fun() -> ets:lookup_element(T, 2, 3) end)]).

%% `match_delete/2` on a multi kind removes EVERY matching object — a
%% duplicate_bag loses both copies in one call — and answers `true`.
multi_match_delete(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    ets:insert(T, {2, 30}),
    Before = tsize(T),
    C = cap(fun() -> ets:match_delete(T, {1, '_'}) end),
    t([C, r(Before), r(tsize(T)), srt(ets:tab2list(T))]).

%% `select_delete/2` on a multi kind reports the number of OBJECTS removed, so a
%% duplicate_bag's duplicate copies each count.
multi_select_delete(Kind) ->
    T = new(Kind),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    ets:insert(T, {2, 30}),
    C = cap(fun() -> ets:select_delete(T, [{{1, '_'}, [], [true]}]) end),
    t([C, srt(ets:tab2list(T)), r(tsize(T))]).

%% A duplicate_bag select_replace whose body changes the key is `badarg` and
%% leaves the table unchanged.
duplicate_bag_select_replace_changed_key() ->
    T = new(duplicate_bag),
    ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
    C = cap(fun() -> ets:select_replace(T, [{{'$1', '$2'}, [], [{{2, '$2'}}]}]) end),
    t([C, srt(ets:tab2list(T))]).

%% A {keypos, 2} multi kind indexes by the second element: a bag still rejects
%% an identical object, a duplicate_bag keeps it.
multi_kind_keypos2(Kind) ->
    T = new(Kind, 2),
    ets:insert(T, {a, 1}), ets:insert(T, {b, 1}), ets:insert(T, {c, 2}),
    S0 = tsize(T),
    ets:insert(T, {a, 1}),
    t([r(S0), srt(ets:lookup(T, 1)), srt(ets:lookup(T, 2)),
       r(ets:member(T, 1)), r(tsize(T))]).
%% ------------------------------ update_counter ------------------------------

update_counter_3_variants() ->
    T = new(set),
    ets:insert(T, {1, 10, 20}),
    V1 = ets:update_counter(T, 1, 5),
    V2 = ets:update_counter(T, 1, -3),
    V3 = ets:update_counter(T, 1, {3, 5}),
    V4 = ets:update_counter(T, 1, [{2, 1}, {3, 1}]),
    t([r(V1), r(V2), r(V3), r(V4), srt(ets:lookup(T, 1))]).

update_counter_3_absent() ->
    T = new(set),
    t([cap(fun() -> ets:update_counter(T, 1, 5) end),
       cap(fun() -> ets:update_counter(T, 1, {2, 1}) end),
       r(tsize(T))]).

update_counter_3_threshold() ->
    Ops = [{2, 5, 12, 100}, {2, -9, 3, 100}],
    l([begin
           T = new(set),
           ets:insert(T, {1, 10}),
           cap(fun() -> ets:update_counter(T, 1, Op) end)
       end || Op <- Ops]).

update_counter_4_default() ->
    T = new(set),
    V1 = ets:update_counter(T, 1, 5, {1, 0, 0}),
    L1 = ets:lookup(T, 1),
    V2 = ets:update_counter(T, 1, 5, {1, 0, 0}),
    V3 = ets:update_counter(T, 7, 3, {7, 0, 0}),
    L3 = ets:lookup(T, 7),
    t([r(V1), srt(L1), r(V2), r(V3), srt(L3)]).

update_counter_4_list() ->
    T = new(set),
    ets:insert(T, {1, 10, 20}),
    V1 = ets:update_counter(T, 1, [{2, 1}, {3, 1}], {1, 0, 0}),
    L1 = ets:lookup(T, 1),
    V2 = ets:update_counter(T, 1, [{2, 5}], {1, 0, 0}),
    V3 = ets:update_counter(T, 9, [], {9, 0, 0}),
    L9 = ets:lookup(T, 9),
    t([r(V1), srt(L1), r(V2), r(V3), srt(L9)]).

update_counter_4_threshold() ->
    Ops = [{2, 5, 12, 100}, {2, 2, 12, 100}, {2, -9, 3, 100},
           {2, -7, 3, 100}, {2, 0, 5, 100}],
    l([begin
           T = new(set),
           ets:insert(T, {1, 10}),
           V = ets:update_counter(T, 1, Op, {1, 0}),
           t([r(V), srt(ets:lookup(T, 1))])
       end || Op <- Ops]).

%% `{Pos, Incr, Threshold, SetValue}` combined with a `/4` default object for an
%% absent key: a plain increment stays below the threshold, crossing it upward
%% (`>=` for a positive increment) and downward (`=<` for a negative one) lands on
%% `SetValue`, and a value exactly at the threshold is left alone.
update_counter_threshold_default() ->
    T = new(set),
    V1 = ets:update_counter(T, 1, {2, 5, 100, 0}, {1, 10}),
    L1 = srt(ets:lookup(T, 1)),
    V2 = ets:update_counter(T, 1, {2, 95, 100, -1}),
    L2 = srt(ets:lookup(T, 1)),
    V3 = ets:update_counter(T, 1, {2, 1, 100, 999}),
    L3 = srt(ets:lookup(T, 1)),
    V4 = ets:update_counter(T, 2, {2, -20, 0, -5}, {2, 1}),
    L4 = srt(ets:lookup(T, 2)),
    V5 = ets:update_counter(T, 2, {2, -1, 0, 77}),
    L5 = srt(ets:lookup(T, 2)),
    t([r(V1), L1, r(V2), L2, r(V3), L3, r(V4), L4, r(V5), L5]).
update_counter_multi_kind() ->
    l([begin
           T = new(Kind),
           ets:insert(T, {1, 10}),
           cap(fun() -> ets:update_counter(T, 1, 1) end)
       end || Kind <- [bag, duplicate_bag]]).

update_element_variants() ->
    T = new(set),
    ets:insert(T, {1, 10, 20, 30}),
    R1 = ets:update_element(T, 1, {2, 99}),
    L1 = ets:lookup(T, 1),
    R2 = ets:update_element(T, 1, [{3, 2}, {2, 5}]),
    L2 = ets:lookup(T, 1),
    R3 = ets:update_element(T, 1, []),
    L3 = ets:lookup(T, 1),
    R4 = ets:update_element(T, 99, {2, 9}),
    E1 = cap(fun() -> ets:update_element(T, 1, {1, 9}) end),
    E2 = cap(fun() -> ets:update_element(T, 1, {0, 9}) end),
    E3 = cap(fun() -> ets:update_element(T, 1, {5, 9}) end),
    L4 = ets:lookup(T, 1),
    t([r(R1), srt(L1), r(R2), srt(L2), r(R3), srt(L3), r(R4),
       E1, E2, E3, srt(L4)]).

%% ------------------------------ take / lookup_element ------------------------------

take_and_lookup_element() ->
    T = new(set),
    ets:insert(T, {1, 10, 20}),
    E1 = ets:lookup_element(T, 1, 2),
    E2 = ets:lookup_element(T, 1, 3),
    B1 = cap(fun() -> ets:lookup_element(T, 1, 4) end),
    B2 = cap(fun() -> ets:lookup_element(T, 99, 1) end),
    B3 = cap(fun() -> ets:lookup_element(T, 1, 0) end),
    D1 = ets:lookup_element(T, 99, 2, default),
    D2 = ets:lookup_element(T, 1, 2, default),
    D3 = ets:lookup_element(T, 99, 5, default),
    B4 = cap(fun() -> ets:lookup_element(T, 1, 4, default) end),
    t([r(E1), r(E2), B1, B2, B3, r(D1), r(D2), r(D3), B4]).

%% ------------------------------ select / match ------------------------------

select_head_vars() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    Spec = [{{'$1', '$2'}, [], ['$2']}],
    inorder(ets:select(T, Spec)).

select_wildcard_literal() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}), ets:insert(T, {3, c, 30}),
    Spec = [{{1, '_', '_'}, [], ['$_']}],
    inorder(ets:select(T, Spec)).

select_guard_comparison() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}), ets:insert(T, {3, c, 30}),
    Spec = [{{'$1', '$2', '$3'}, [{'>', '$3', 15}], ['$1']}],
    inorder(ets:select(T, Spec)).

select_guard_typetest() ->
    T = new(ordered_set),
    ets:insert(T, {1, a}), ets:insert(T, {2, 7}),
    Spec = [{{'$1', '$2'}, [{is_integer, '$2'}], ['$1']}],
    inorder(ets:select(T, Spec)).

select_guard_andalso_orelse() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}), ets:insert(T, {3, c, 30}),
    S1 = [{{'$1', '$2', '$3'}, [{'andalso', {'>', '$3', 5}, {'<', '$3', 25}}], ['$1']}],
    S2 = [{{'$1', '$2', '$3'}, [{'orelse', {'=:=', '$1', 1}, {'=:=', '$1', 3}}], ['$1']}],
    t([inorder(ets:select(T, S1)), inorder(ets:select(T, S2))]).

%% The guard/body language the earlier cases miss, all rendered through `cap_str/1`
%% so a raising call is compared as a value: the `/` operator (whose Erlang result
%% is *always* a float) and a float arithmetic operand, `float/1`, `element/2`,
%% `abs/1` over a small integer and over a float, the map guards `map_size/1`,
%% `is_map_key/2` and `map_get/2`, `tuple_size/1`, `byte_size/1`+`bit_size/1`, and a
%% map *constructed in a body*.
select_guard_div() ->
    T = new(ordered_set),
    [ets:insert(T, {K, K * 10}) || K <- [1, 2, 3]],
    S = [{{'$1', '$2'}, [{'==', {'/', '$2', 2}, 10.0}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_float() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20.0}), ets:insert(T, {3, 30}),
    S = [{{'$1', '$2'}, [{'==', {float, '$2'}, 20.0}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_element() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}), ets:insert(T, {3, c, 30}),
    S = [{{'$1', '$2', '$3'}, [{'>', {element, 3, '$_'}, 15}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_abs() ->
    T = new(ordered_set),
    ets:insert(T, {1, -10}), ets:insert(T, {2, 20}), ets:insert(T, {3, -30}),
    S = [{{'$1', '$2'}, [{'>', {abs, '$2'}, 15}], ['$1']}],
    F = new(ordered_set),
    ets:insert(F, {1, -10.0}), ets:insert(F, {2, 20.0}),
    SF = [{{'$1', '$2'}, [{'==', {abs, '$2'}, 10.0}], ['$1']}],
    t([cap_str(fun() -> inorder(ets:select(T, S)) end),
       cap_str(fun() -> inorder(ets:select(F, SF)) end)]).

select_guard_map_size() ->
    T = new(ordered_set),
    ets:insert(T, {1, #{a => 1, b => 2}}), ets:insert(T, {2, #{a => 1}}),
    S = [{{'$1', '$2'}, [{'==', {map_size, '$2'}, 2}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_is_map_key() ->
    T = new(ordered_set),
    ets:insert(T, {1, #{a => 10}}), ets:insert(T, {2, #{b => 20}}),
    S = [{{'$1', '$2'}, [{is_map_key, a, '$2'}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_map_get() ->
    T = new(ordered_set),
    ets:insert(T, {1, #{a => 10}}), ets:insert(T, {2, #{b => 20}}),
    S = [{{'$1', '$2'}, [{'==', {map_get, a, '$2'}, 10}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_tuple_size() ->
    T = new(ordered_set),
    ets:insert(T, {1, {a, b, c}}), ets:insert(T, {2, {a, b}}),
    S = [{{'$1', '$2'}, [{'==', {tuple_size, '$2'}, 3}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_size_family() ->
    T = new(ordered_set),
    ets:insert(T, {1, <<"abc">>}), ets:insert(T, {2, <<"ab">>}),
    S = [{{'$1', '$2'}, [{'andalso', {'==', {byte_size, '$2'}, 3},
                                  {'==', {bit_size, '$2'}, 24}}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_guard_float_arith() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    S = [{{'$1', '$2'}, [{'==', {'*', '$2', 1.5}, 30.0}], ['$1']}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_body_whole() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}),
    Spec = [{{'$1', '$2', '$3'}, [], ['$_']}],
    inorder(ets:select(T, Spec)).

select_body_var() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}),
    Spec = [{{'$1', '$2', '$3'}, [], ['$1']}],
    inorder(ets:select(T, Spec)).

select_body_const() ->
    T = new(ordered_set),
    ets:insert(T, {1, a, 10}), ets:insert(T, {2, b, 20}),
    Spec = [{{'$1', '$2', '$3'}, [], [ok]}],
    inorder(ets:select(T, Spec)).

select_body_construct() ->
    T = new(ordered_set),
    ets:insert(T, {1, a}), ets:insert(T, {2, b}),
    Spec = [{{'$1', '$2'}, [], [{{'$1', '$2'}}]}],
    inorder(ets:select(T, Spec)).

%% A map *constructed in a body* from the bound variables, then read back with
%% `map_get/2` so the clause value stays a small integer (the renderer has no map
%% clause).
select_body_map_construct() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    S = [{{'$1', '$2'}, [], [{map_get, '$1', #{'$1' => '$2'}}]}],
    cap_str(fun() -> inorder(ets:select(T, S)) end).

select_delete_and_count() ->
    T = new(ordered_set),
    [ets:insert(T, {K, a, K * 10}) || K <- [1, 2, 3]],
    TrueSpec = [{{'$1', '$2', '$3'}, [{'>', '$3', 15}], [true]}],
    FalseSpec = [{{'$1', '$2', '$3'}, [{'>', '$3', 15}], [false]}],
    C0 = ets:select_count(T, FalseSpec),
    D0 = ets:select_delete(T, FalseSpec),
    C1 = ets:select_count(T, TrueSpec),
    D1 = ets:select_delete(T, TrueSpec),
    Remaining = [element(1, O) || O <- ets:tab2list(T)],
    t([r(C0), r(D0), r(C1), r(D1), r(tsize(T)), inorder(Remaining)]).

match_family() ->
    T = new(ordered_set),
    [ets:insert(T, {K, a, K * 10}) || K <- [1, 2, 3]],
    MO = ets:match_object(T, {'$1', '_', '$3'}),
    M = ets:match(T, {'$1', '_', '$3'}),
    Before = tsize(T),
    D = ets:match_delete(T, {1, '_', '_'}),
    After = tsize(T),
    t([inorder(MO), inorder(M), r(D), r(Before), r(After),
       inorder([element(1, O) || O <- ets:tab2list(T)])]).

malformed_specs() ->
    T = new(set),
    ets:insert(T, {1, 10}),
    Cases = [1,
             [a_list],
             [{'_', []}],
             [{'_', [], []}],
             [{{'$1'}, [{is_foo, '$1'}], ['$_']}],
             [{'_', [], ['$2']}],
             [{'_', [{is_integer, '$1'}], ['$_']}],
             [{'_', [], ['$_']}]],
    l([cap(fun() -> ets:select(T, C) end) || C <- Cases]).

%% --------------------- info / new-option surface ---------------------

%% The registered `name` and the `named_table` flag are independent: a name is
%% recorded whether or not `named_table` was requested, and only the option sets
%% the flag. A `named_table` registers its name globally in the oracle VM, so the
%% named table below uses a unique atom and is created exactly once.
info_name_named_table() ->
    Anon = ets:new(anon_case_name, []),
    N1 = ets:info(Anon, name),
    B1 = ets:info(Anon, named_table),
    Named = ets:new(named_case_name, [named_table]),
    N2 = ets:info(Named, name),
    B2 = ets:info(Named, named_table),
    t([r(N1), r(B1), r(N2), r(B2)]).

%% The renderable `ets:info/2` fields for four option combinations. `id`,
%% `memory`, `owner` and `node` are implementation-specific and never rendered.
info_option_fields() ->
    T1 = ets:new(diff, []),
    T2 = ets:new(diff, [{keypos, 2}]),
    T3 = ets:new(diff, [ordered_set, public, compressed,
                        {write_concurrency, true}, {read_concurrency, true}]),
    T4 = ets:new(diff, [private, {write_concurrency, true},
                        {read_concurrency, true}]),
    t([info_opts(T1), info_opts(T2), info_opts(T3), info_opts(T4)]).

info_opts(T) ->
    t([r(ets:info(T, name)), r(ets:info(T, named_table)), r(ets:info(T, type)),
       r(ets:info(T, keypos)), r(ets:info(T, protection)),
       r(ets:info(T, compressed)), r(ets:info(T, write_concurrency)),
       r(ets:info(T, read_concurrency))]).

%% The three `ets:info/1` fields fixed by the runtime shape rather than by the
%% table, read for a plain `set` and for a public `ordered_set` with
%% `write_concurrency`: `heir` (always `none`), `node` (always `nonode@nohost`,
%% this VM is never distributed) and `decentralized_counters` (`true` iff an
%% `ordered_set` has `write_concurrency` on — the CATREE path).
info_static_fields() ->
    Set = new(set),
    Ordered = ets:new(diff, [ordered_set, public, {write_concurrency, true}]),
    t([r(ets:info(Set, heir)), r(ets:info(Set, node)),
       r(ets:info(Set, decentralized_counters)),
       r(ets:info(Ordered, heir)), r(ets:info(Ordered, node)),
       r(ets:info(Ordered, decentralized_counters))]).

%% `ets:info/2` rejects a key it does not report.
info_unknown_key() ->
    T = new(set),
    cap(fun() -> ets:info(T, no_such_key) end).

%% `ets:lookup_element/3` on a multi-object kind answers with the *list* of the
%% `Pos`-th element of every object under `Key` (a `set` stays scalar); an absent
%% key is `badarg` (`/3`) or the default (`/4`), `Pos == 0` is `badarg`, and one
%% object too short for `Pos` makes the whole call `badarg`.
lookup_element_multi() ->
    Bag = new(bag),
    ets:insert(Bag, {1, 10}), ets:insert(Bag, {1, 20}),
    BagE = ets:lookup_element(Bag, 1, 2),
    One = new(bag),
    ets:insert(One, {2, 7}),
    OneE = ets:lookup_element(One, 2, 1),
    Dup = new(duplicate_bag),
    ets:insert(Dup, {1, 10}), ets:insert(Dup, {1, 10}), ets:insert(Dup, {1, 20}),
    DupE = ets:lookup_element(Dup, 1, 2),
    Set = new(set),
    ets:insert(Set, {1, 10}),
    SetE = ets:lookup_element(Set, 1, 2),
    Absent3 = cap(fun() -> ets:lookup_element(Bag, 99, 2) end),
    Absent4 = ets:lookup_element(Bag, 99, 2, default),
    Pos0 = cap(fun() -> ets:lookup_element(Bag, 1, 0) end),
    Short = new(bag),
    ets:insert(Short, {3, 5, 6}), ets:insert(Short, {3, 7}),
    ShortErr = cap(fun() -> ets:lookup_element(Short, 3, 3) end),
    t([r(BagE), r(OneE), r(DupE), r(SetE), Absent3, r(Absent4), Pos0, ShortErr]).

%% `ets:insert/2` over a list is atomic: one invalid element makes the whole
%% call `badarg` and inserts none of the elements, not even the valid ones that
%% precede it.
insert_list_atomic() ->
    T = new(set),
    ets:insert(T, {1, 10}),
    R1 = cap(fun() -> ets:insert(T, [{2, 20}, foo]) end),
    L1 = srt(ets:tab2list(T)),
    T2 = new(set, 2),
    ets:insert(T2, {a, 1}),
    R2 = cap(fun() -> ets:insert(T2, [{b, 2}, {c}]) end),
    L2 = srt(ets:tab2list(T2)),
    t([R1, L1, R2, L2]).

%% Repeated insert/replace/delete churn and a `delete_all_objects/1` must leave
%% only an *observable* trace — `size`, `member/2`, `lookup/2` and `tab2list/1`.
%% `memory` and the table `id` are implementation-specific and are never rendered.
reclamation_observable() ->
    T = new(set),
    lists:foreach(fun(_) ->
        [ets:insert(T, {K, K}) || K <- lists:seq(1, 10)],
        [ets:delete(T, K) || K <- lists:seq(1, 10, 2)]
    end, lists:seq(1, 5)),
    S1 = r(tsize(T)),
    M = l([r(ets:member(T, K)) || K <- [1, 2, 10, 11]]),
    L = srt(ets:lookup(T, 4)),
    ets:delete_all_objects(T),
    S2 = r(tsize(T)),
    L2 = srt(ets:tab2list(T)),
    ets:insert(T, {7, 70}),
    S3 = r(tsize(T)),
    L3 = srt(ets:lookup(T, 7)),
    t([S1, M, L, S2, L2, S3, L3]).
%% --------------------- match_spec_run / select_replace / continuation ---------------------

%% `ets:match_spec_run/2` requires the spec already compiled by
%% `ets:match_spec_compile/1` (a raw clause list is `badarg` there).
msc(S) -> ets:match_spec_compile(S).

%% The all-bindings atom `'$$'` has two characters; written literally as `'$$'`
%% the Erlang lexer reads the one-character atom `'$'`, so build it explicitly.
msr_all_bindings() -> list_to_atom([36, 36]).

match_spec_run_bindings() ->
    L = [{1, 10}, {2, 20}, {3, 30}],
    S1 = [{{'$1', '$2'}, [], ['$1']}],
    S2 = [{{'$1', '$2'}, [], ['$_']}],
    S3 = [{{'$1', '$2'}, [], [msr_all_bindings()]}],
    t([inorder(ets:match_spec_run(L, msc(S1))),
       inorder(ets:match_spec_run(L, msc(S2))),
       inorder(ets:match_spec_run(L, msc(S3)))]).

match_spec_run_repeated_var() ->
    L = [{1, 1}, {1, 2}, {2, 2}],
    S = [{{'$1', '$1'}, [], ['$1']}],
    inorder(ets:match_spec_run(L, msc(S))).

match_spec_run_multi_clause() ->
    L = [{1, 10}, {2, 20}, {3, 30}, 42],
    S = [{{'$1', '$2'}, [{'>', '$2', 15}], [big]},
         {{'$1', '$2'}, [], [small]}],
    inorder(ets:match_spec_run(L, msc(S))).

match_spec_run_empty_and_badarg() ->
    S = [{{'$1', '$2'}, [], ['$1']}],
    E = ets:match_spec_run([], msc(S)),
    B = cap(fun() -> ets:match_spec_run(foo, msc(S)) end),
    t([inorder(E), B]).

select_replace_small_int() ->
    T = new(set),
    [ets:insert(T, {K, K * 10}) || K <- [1, 2, 3]],
    S = [{{'$1', '$2'}, [{'>', '$2', 10}], [{{'$1', {'*', '$2', 2}}}]}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

select_replace_arity1() ->
    T = new(set),
    ets:insert(T, {1, a}), ets:insert(T, {2, b}),
    S = [{{'$1', '$2'}, [], [{{'$1'}}]}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

select_replace_bag_badarg() ->
    T = new(bag),
    ets:insert(T, {1, 10}),
    S = [{{'$1', '$2'}, [], [{{'$1', '$2'}}]}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

select_replace_changed_key_badarg() ->
    T = new(set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    S = [{{'$1', '$2'}, [], [{{99, '$2'}}]}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

select_replace_identity() ->
    T = new(set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    S = [{{'$1', '$2'}, [], ['$_']}],
    C = cap(fun() -> ets:select_replace(T, S) end),
    t([C, srt(ets:tab2list(T))]).

%% The 5-object `ordered_set` `{1,x}`..`{5,x}` the continuation cases chain over.
chain_table() ->
    T = new(ordered_set),
    [ets:insert(T, {K, x}) || K <- [1, 2, 3, 4, 5]],
    T.

select_limit_chain() ->
    T = chain_table(),
    S = [{{'$1', '$2'}, [], ['$1']}],
    inorder(chain_select(T, S, 2)).

chain_select(T, S, Limit) ->
    case ets:select(T, S, Limit) of
        '$end_of_table' -> [];
        {Rs, Cont} -> Rs ++ chain_select_cont(Cont)
    end.

chain_select_cont(Cont) ->
    case ets:select(Cont) of
        '$end_of_table' -> [];
        {Rs, Cont2} -> Rs ++ chain_select_cont(Cont2)
    end.

match_limit_chain() ->
    T = chain_table(),
    inorder(chain_match(T, {'$1', '$2'}, 2)).

chain_match(T, P, Limit) ->
    case ets:match(T, P, Limit) of
        '$end_of_table' -> [];
        {Rs, Cont} -> Rs ++ chain_match_cont(Cont)
    end.

chain_match_cont(Cont) ->
    case ets:match(Cont) of
        '$end_of_table' -> [];
        {Rs, Cont2} -> Rs ++ chain_match_cont(Cont2)
    end.

match_object_limit_chain() ->
    T = chain_table(),
    inorder(chain_mo(T, {'$1', '$2'}, 2)).

chain_mo(T, P, Limit) ->
    case ets:match_object(T, P, Limit) of
        '$end_of_table' -> [];
        {Rs, Cont} -> Rs ++ chain_mo_cont(Cont)
    end.

chain_mo_cont(Cont) ->
    case ets:match_object(Cont) of
        '$end_of_table' -> [];
        {Rs, Cont2} -> Rs ++ chain_mo_cont(Cont2)
    end.

select_limit_zero_badarg() ->
    T = chain_table(),
    S = [{{'$1', '$2'}, [], ['$1']}],
    cap(fun() -> ets:select(T, S, 0) end).

select_limit_empty_table() ->
    T = new(ordered_set),
    S = [{{'$1', '$2'}, [], ['$1']}],
    r(ets:select(T, S, 2)).

body_error_exit() ->
    T = new(ordered_set),
    ets:insert(T, {1, 10}), ets:insert(T, {2, 20}),
    S = [{{'_', '_'}, [], [{hd, 5}]}],
    inorder(ets:select(T, S)).

%% The whole `select` family over a hand-built `fun2ms`-shaped spec — the
%% `[{{Head}, [Guards], [Body]}]` shape `ets:fun2ms/1` emits — exercising every
%% special body form: a bound variable `'$N'`, the whole object `'$_'` and the
%% all-bindings atom `'$$'` (built by `msr_all_bindings/0`, since a literal `'$$'`
%% lexes to the one-character `'$'`). `select_count/2` counts only objects whose
%% body evaluates to `true`, so it needs a `[true]` body (not the `'$1'` body the
%% plain `select/2` uses). `select_replace/2` and `select_delete/2` are pinned on
%% their own fresh tables because they mutate.
fun2ms_spec_family() ->
    T = new(ordered_set),
    [ets:insert(T, {K, K, K * 10}) || K <- [1, 2, 3]],
    Sel = [{{'$1', '$2', '$3'}, [{'>', '$3', 15}], ['$1']}],
    R1 = inorder(ets:select(T, Sel)),
    CountTrue = [{{'$1', '$2', '$3'}, [{'>', '$3', 15}], [true]}],
    R2 = r(ets:select_count(T, CountTrue)),
    MO = inorder(ets:match_object(T, {'$1', '_', '$3'})),
    M = inorder(ets:match(T, {'$1', '_', '$3'})),
    Whole = [{{'$1', '_', '$3'}, [], ['$_']}],
    R3 = inorder(ets:select(T, Whole)),
    All = [{{'$1', '$2', '$3'}, [], [msr_all_bindings()]}],
    R4 = inorder(ets:select(T, All)),
    R5 = inorder(ets:match_spec_run([{1, 2}, {3, 4}],
                                    msc([{{'$1', '$2'}, [], ['$_']}]))),
    Tr = new(ordered_set),
    [ets:insert(Tr, {K, K * 10}) || K <- [1, 2, 3]],
    Replace = [{{'$1', '$2'}, [{'>', '$2', 10}], [{{'$1', {'*', '$2', 2}}}]}],
    RC = cap(fun() -> ets:select_replace(Tr, Replace) end),
    RT = srt(ets:tab2list(Tr)),
    Td = new(ordered_set),
    [ets:insert(Td, {K, K * 10}) || K <- [1, 2, 3]],
    Del = [{{'$1', '$2'}, [{'>', '$2', 10}], [true]}],
    DC = r(ets:select_delete(Td, Del)),
    DT = srt(ets:tab2list(Td)),
    t([R1, R2, MO, M, R3, R4, R5, RC, RT, DC, DT]).
%% --------------------- ownership: give_away / heir ---------------------

ownership_give_away() ->
    Parent = self(),
    Child = spawn(fun() -> receive {go, From} -> From ! ok; stop -> ok end end),
    T1 = ets:new(diff, [set, public]),
    R1 = cap(fun() -> ets:give_away(T1, Child, gift) end),
    R2 = cap(fun() -> ets:give_away(T1, Parent, gift) end),
    T2 = ets:new(diff, [set, public]),
    R3 = cap(fun() -> ets:give_away(T2, self(), gift) end),
    T3 = ets:new(diff, [set, public]),
    R4 = cap(fun() -> ets:give_away(T3, not_a_pid, gift) end),
    Child ! stop,
    t([R1, R2, R3, R4]).

ownership_heir() ->
    Heir = spawn(fun() -> receive stop -> ok end end),
    T = ets:new(diff, [set, public]),
    H0 = r(ets:info(T, heir)),
    S1 = cap(fun() -> ets:setopts(T, {heir, Heir, gift}) end),
    S2 = cap(fun() -> ets:setopts(T, {heir, none}) end),
    H1 = r(ets:info(T, heir)),
    Hold = cap(fun() -> ets:info(T, holder) end),
    Heir ! stop,
    t([H0, S1, S2, H1, Hold]).

%% --------------------- access control: protection matrix ---------------------

%% The `Db_READ`/`Db_WRITE` protection matrix of `db_get_table_aux`
%% (`erl_db.c`), asserted for the *whole* observable operation set: the
%% read-gated operations (`lookup`/`tab2list`/`first`/`next`/`select`/
%% `match_object`, plus `safe_fixtable/2`), the write-gated ones
%% (`insert`/`insert_new`/`delete`/`delete_object`/`delete_all_objects`/`take`/
%% `select_delete`), and the **ungated** introspection (`ets:info/1` and
%% `ets:info/2` succeed even for a non-owner of a `private` table).
%%
%% Each operation runs on its *own* freshly created, owner-seeded `ordered_set` —
%% created and owned by a spawned child, so the battery process is a *non-owner* —
%% so a mutation can never perturb a later operation's observable. The
%% `ordered_set` kind makes `first`/`next` deterministic (a `set`'s navigation
%% order is unspecified).
access_control() ->
    t([access_case(protected), access_case(private), access_case(public)]).

access_case(Prot) ->
    Ops = access_ops(),
    t([t([access_probe(Prot, owner, Op) || Op <- Ops]),
       t([access_probe(Prot, non_owner, Op) || Op <- Ops])]).

%% The operation order both sides must share: the reads (incl. the read-gated
%% `safe_fixtable/2` and the ungated `ets:info/1,2`) first, then the writes.
access_ops() ->
    [fun(T) -> r(ets:lookup(T, 1)) end,
     fun(T) -> srt(ets:tab2list(T)) end,
     fun(T) -> r(ets:first(T)) end,
     fun(T) -> r(ets:next(T, 1)) end,
     fun(T) -> srt(ets:select(T, [{{'_', '_'}, [], ['$_']}])) end,
     fun(T) -> srt(ets:match_object(T, {'_', '_'})) end,
     fun(T) -> r(ets:safe_fixtable(T, true)) end,
     fun(T) -> srt([K || {K, _} <- ets:info(T)]) end,
     fun(T) -> r(ets:info(T, size)) end,
     fun(T) -> r(ets:insert(T, {3, 30})) end,
     fun(T) -> r(ets:insert_new(T, {1, 99})) end,
     fun(T) -> r(ets:delete(T, 1)) end,
     fun(T) -> r(ets:delete_object(T, {1, 10})) end,
     fun(T) -> r(ets:delete_all_objects(T)) end,
     fun(T) -> r(ets:take(T, 1)) end,
     fun(T) -> r(ets:select_delete(T, [{{'_', '_'}, [], [true]}])) end].

%% Run `Op` on a fresh, owner-seeded `ordered_set` as `Who`: `owner` runs it in
%% the owning child process, `non_owner` in the battery process. A `badarg` (or
%% any other raise) is rendered by `cap_str/1`.
access_probe(Prot, Who, Op) ->
    Parent = self(),
    spawn(fun() ->
        T = ets:new(diff, [ordered_set, Prot]),
        ets:insert(T, {1, 10}),
        ets:insert(T, {2, 20}),
        Own = case Who of
                  owner -> cap_str(fun() -> Op(T) end);
                  non_owner -> none
              end,
        Parent ! {self(), T, Own},
        receive stop -> ok end
    end),
    receive {Child, T, Own} ->
        R = case Who of
                owner -> Own;
                non_owner -> cap_str(fun() -> Op(T) end)
            end,
        Child ! stop,
        R
    end.
%% --------------------- setopts/2 surface ---------------------

%% `ets:setopts/2` accepts only `{heir, Pid, Data}` / `{heir, none}` on OTP 29:
%% every concurrency option and any unknown option raises `badarg`, and a
%% non-owner `setopts` raises `badarg`.
setopts_surface() ->
    Heir = spawn(fun() -> receive stop -> ok end end),
    T = ets:new(diff, [set, public]),
    S1 = cap(fun() -> ets:setopts(T, {heir, Heir, gift}) end),
    S2 = cap(fun() -> ets:setopts(T, {heir, none}) end),
    S3 = cap(fun() -> ets:setopts(T, {write_concurrency, true}) end),
    S4 = cap(fun() -> ets:setopts(T, {read_concurrency, true}) end),
    S5 = cap(fun() -> ets:setopts(T, {decentralized_counters, true}) end),
    S6 = cap(fun() -> ets:setopts(T, {no_such_option, true}) end),
    Parent = self(),
    spawn(fun() ->
        Parent ! {non_owner, cap(fun() -> ets:setopts(T, {heir, none}) end)}
    end),
    NonOwner = receive {non_owner, R} -> R end,
    Heir ! stop,
    t([S1, S2, S3, S4, S5, S6, NonOwner]).
%% --------------------- update_counter with bignums ---------------------

update_counter_bignum_overflow() ->
    T = new(set),
    M = (1 bsl 59) - 1,
    ets:insert(T, {1, M}),
    V1 = ets:update_counter(T, 1, 1),
    L1 = ets:lookup(T, 1),
    V2 = ets:update_counter(T, 1, -1),
    t([r(V1), srt(L1), r(V2), srt(ets:lookup(T, 1))]).

update_counter_bignum_kinds() ->
    l([begin
           T = new(Kind),
           ets:insert(T, {1, (1 bsl 59) - 1}),
           V1 = ets:update_counter(T, 1, 1),
           V2 = ets:update_counter(T, 1, 1 bsl 100),
           t([r(V1), r(V2), srt(ets:lookup(T, 1))])
       end || Kind <- [set, ordered_set]]).

update_counter_bignum_operand() ->
    T = new(set),
    B = 1 bsl 100,
    ets:insert(T, {1, 10}),
    V1 = ets:update_counter(T, 1, B),
    V2 = ets:update_counter(T, 1, -B),
    t([r(V1), r(V2), srt(ets:lookup(T, 1))]).

update_counter_bignum_element() ->
    T = new(set),
    B = -(1 bsl 100),
    ets:insert(T, {1, B}),
    V1 = ets:update_counter(T, 1, 5),
    V2 = ets:update_counter(T, 1, -3),
    t([r(V1), r(V2), srt(ets:lookup(T, 1))]).

update_counter_bignum_add() ->
    T = new(set),
    A = 1 bsl 200,
    B = (1 bsl 192) + 7,
    ets:insert(T, {1, A}),
    V1 = ets:update_counter(T, 1, B),
    V2 = ets:update_counter(T, 1, -A),
    t([r(V1), r(V2), srt(ets:lookup(T, 1))]).

update_counter_bignum_mixed_sign() ->
    T = new(set),
    A = 1 bsl 100,
    ets:insert(T, {1, -A}),
    V1 = ets:update_counter(T, 1, A),
    V2 = ets:update_counter(T, 1, -(A + 1)),
    V3 = ets:update_counter(T, 1, A),
    t([r(V1), r(V2), r(V3), srt(ets:lookup(T, 1))]).

update_counter_bignum_threshold() ->
    Ops = [{2, 1 bsl 100, 1 bsl 99, 7},
           {2, -(1 bsl 100), -(1 bsl 99), -7},
           {2, 1 bsl 100, 1 bsl 101, 0}],
    l([begin
           T = new(set),
           ets:insert(T, {1, 0}),
           V = ets:update_counter(T, 1, Op),
           t([r(V), srt(ets:lookup(T, 1))])
       end || Op <- Ops]).

update_counter_bignum_warp_value() ->
    T = new(set),
    ets:insert(T, {1, 10}),
    V = ets:update_counter(T, 1, {2, 1, 5, 1 bsl 100}),
    t([r(V), srt(ets:lookup(T, 1))]).

update_counter_bignum_list_ops() ->
    T = new(set),
    ets:insert(T, {1, 0, 0}),
    V = ets:update_counter(T, 1, [{2, 1 bsl 100}, {3, -(1 bsl 100)}]),
    t([r(V), srt(ets:lookup(T, 1))]).

update_counter_bignum_position_badarg() ->
    T = new(set),
    ets:insert(T, {1, 10}),
    P = 1 bsl 100,
    C1 = cap(fun() -> ets:update_counter(T, 1, {P, 1}) end),
    C2 = cap(fun() -> ets:update_counter(T, 1, {2, 1, P, 5}) end),
    C3 = cap(fun() -> ets:update_counter(T, 1, nope) end),
    C4 = cap(fun() -> ets:update_counter(T, 1, {2, nope}) end),
    t([C1, C2, C3, C4, srt(ets:lookup(T, 1))]).

%% ------------------------------ large tables ------------------------------

set_large() ->
    N = 10001,
    T = new(set),
    [ets:insert(T, {I, I * 2}) || I <- lists:seq(0, N - 1)],
    Ls = [ets:lookup(T, K) || K <- [0, 5000, 10000, 10001]],
    Count = ets:select_count(T, [{{'$1', '$2'}, [{'>', '$2', 10000}], [true]}]),
    t([r(tsize(T)), srt(lists:append(Ls)), r(Count)]).

ordered_large() ->
    N = 10001,
    T = new(ordered_set),
    [ets:insert(T, {I, I * 2}) || I <- lists:seq(0, N - 1)],
    WalkLen = length(walk_next(T, ets:first(T), [])),
    Ls = [ets:lookup(T, K) || K <- [0, 5000, 10000, 10001]],
    Count = ets:select_count(T, [{{'$1', '$2'}, [{'>', '$2', 10000}], [true]}]),
    t([r(tsize(T)), r(ets:first(T)), r(ets:last(T)), r(WalkLen),
       r(ets:next(T, 5000)), r(ets:next(T, 10000)),
       srt(lists:append(Ls)), r(Count)]).
"#;
