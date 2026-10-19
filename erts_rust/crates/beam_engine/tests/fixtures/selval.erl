-module(selval).
-export([ints/1, atoms/1, mixed/1]).

ints(X) ->
    case X of
        0 -> zero;
        1 -> one;
        2 -> two;
        3 -> three;
        4 -> four;
        5 -> five;
        _ -> many
    end.

atoms(X) ->
    case X of
        alpha -> 1;
        beta -> 2;
        gamma -> 3;
        delta -> 4;
        _ -> other
    end.

mixed(X) ->
    case X of
        alpha -> a;
        beta -> b;
        0 -> zero;
        1 -> one;
        -1 -> minus_one;
        [] -> empty;
        _ -> other
    end.
