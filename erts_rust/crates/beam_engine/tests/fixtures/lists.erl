%% Oracle fixture for the list instruction family (`beam_engine`).
%%
%% Exercises `put_list`, `get_list`, `get_hd` and `get_tl` with idiomatic
%% Erlang: cons construction, and `case` matches that split a cons cell.
-module(lists).

-export([build/2, head/1, tail/1, split/1]).

%% put_list: build a cons cell (with a proper list tail).
build(H, T) ->
    [H | T].

%% get_hd: match a non-empty list and return its head.
head(L) ->
    case L of
        [H | _T] -> H
    end.

%% get_tl: match a non-empty list and return its tail.
tail(L) ->
    case L of
        [_H | T] -> T
    end.

%% get_list: split a cons cell into its head and tail.
split(L) ->
    case L of
        [H | T] -> {H, T}
    end.
