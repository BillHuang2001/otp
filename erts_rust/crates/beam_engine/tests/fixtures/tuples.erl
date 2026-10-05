%% Oracle fixture for the tuple instruction family (`beam_engine`).
%%
%% Exercises `put_tuple2`, `get_tuple_element` and `select_tuple_arity` with
%% idiomatic Erlang: a tuple constructor, `case` matches that read tuple
%% elements, and a `case` switching on the tuple size.
-module(tuples).

-export([mk/2, first/1, swap/1, classify/1]).

%% put_tuple2: build a fresh two-element tuple.
mk(A, B) ->
    {A, B}.

%% get_tuple_element: match a two-element tuple and return its head.
%% (`case` so a bad argument lands on `case_end`, not a clause-failure jump.)
first(T) ->
    case T of
        {A, _B} -> A
    end.

%% get_tuple_element (twice) + put_tuple2: rebuild with the elements swapped.
swap(T) ->
    case T of
        {A, B} -> {B, A}
    end.

%% select_tuple_arity: case on the tuple's size.
classify(T) ->
    case T of
        {a} -> 1;
        {a, b} -> 2;
        {a, b, c} -> 3
    end.
