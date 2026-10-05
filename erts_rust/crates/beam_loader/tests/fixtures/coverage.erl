%% Test fixture for the `beam_loader` crate: exercises a broader slice of the
%% BEAM instruction set than `slice.erl` (tuples, lists, anonymous funs/closures,
%% `case`, `try`/`catch`, floats, maps and bitstrings). Ordinary idiomatic Erlang
%% so its compiled output is representative of real code.
-module(coverage).
-export([tuple_ops/1, list_ops/1, make_adder/1, apply_adder/2, classify/1,
         safe_div/2, float_ops/2, map_ops/2, bits/1]).

%% Tuples: construction + element access + matching on tuple size.
tuple_ops({A, B}) ->
    {B, A, A + B};
tuple_ops({A, B, C}) ->
    {C, B, A, C - A};
tuple_ops(T) when is_tuple(T) ->
    element(1, T).

%% Lists: construction, head/tail, recursion.
list_ops([H | T]) ->
    [H * 2 | list_ops(T)];
list_ops([]) ->
    [].

%% Anonymous fun capturing a free variable -> make_fun3 / call_fun + FunT chunk.
make_adder(N) ->
    fun(X) -> X + N end.

%% Invoking an anonymous fun -> call_fun.
apply_adder(N, X) ->
    Adder = make_adder(N),
    Adder(X).

%% `case` over patterns.
classify(0) -> zero;
classify(N) when is_integer(N), N > 0 -> positive;
classify(N) when is_integer(N) -> negative;
classify(_) -> other.

%% `try`/`catch`.
safe_div(_A, 0) ->
    {error, division_by_zero};
safe_div(A, B) ->
    try A / B of
        Q -> {ok, Q}
    catch
        error:badarith -> {error, badarith}
    end.

%% Float arithmetic.
float_ops(A, B) ->
    Sum = A + B,
    Diff = A - B,
    Prod = A * B,
    Sum + Diff + Prod.

%% Maps: construct, update with `=>`, update with `:=`, lookup with maps:get.
map_ops(Key, Value) ->
    M0 = #{a => 1, b => 2},
    M1 = M0#{c => Value},
    M2 = M1#{a := maps:get(c, M1) + 10},
    maps:get(Key, M2, 0).

%% Bitstrings: construction and matching.
bits(<<H:8, Rest/binary>>) ->
    <<Rest/binary, H:8>>;
bits(Bin) ->
    Bin.
