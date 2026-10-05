%% Oracle fixture for the map instruction family (`beam_engine`).
%%
%% Exercises `get_map_elements`, `has_map_fields`, `put_map_assoc` and
%% `put_map_exact` with idiomatic Erlang. `case` (rather than function-head
%% clauses) keeps a bad argument on the `case_end`/guard path.
-module(mapfx).

-export([get/1, has/1, assoc/1, exact/1, assoc_exact/1]).

%% get_map_elements: match two map keys and return their values.
get(M) ->
    case M of
        #{a := A, b := B} -> {A, B}
    end.

%% has_map_fields: test for the presence of two keys.
has(M) ->
    case M of
        #{a := _, b := _} -> yes;
        _ -> no
    end.

%% put_map_assoc: associate a new key.
assoc(M) ->
    M#{c => 3}.

%% put_map_exact: update an existing key.
exact(M) ->
    M#{a := 9}.

%% both updates back to back.
assoc_exact(M) ->
    M#{a := 9, c => 3}.
