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

    emit("update_counter_3_variants", update_counter_3_variants()),
    emit("update_counter_3_absent", update_counter_3_absent()),
    emit("update_counter_3_threshold", update_counter_3_threshold()),
    emit("update_counter_4_default", update_counter_4_default()),
    emit("update_counter_4_list", update_counter_4_list()),
    emit("update_counter_4_threshold", update_counter_4_threshold()),
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
    emit("select_body_whole", select_body_whole()),
    emit("select_body_var", select_body_var()),
    emit("select_body_const", select_body_const()),
    emit("select_body_construct", select_body_construct()),
    emit("select_delete_and_count", select_delete_and_count()),
    emit("match_family", match_family()),
    emit("malformed_specs", malformed_specs()),

    emit("info_name_named_table", info_name_named_table()),
    emit("info_option_fields", info_option_fields()),
    emit("info_static_fields", info_static_fields()),
    emit("info_unknown_key", info_unknown_key()),
    emit("lookup_element_multi", lookup_element_multi()),
    emit("insert_list_atomic", insert_list_atomic()),

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
