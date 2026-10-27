%% The `erlang:fun_info_mfa/1` / `erlang:fun_info/2` fixture (`tests/fun_info_mfa.rs`).
%%
%% Every exported function answers one shape of the introspection surface for a
%% **compiled module's own** fun, so the suite can pin the exact `{Module, Name,
%% Arity}` triple the oracle derives from the `FunT` entry:
%%
%%   single/0        one anonymous fun in a function of its own
%%   siblings/0      two anonymous funs in the **same** function (the `-fun-0-`/
%%                   `-fun-1-` numbering)
%%   spread/0        two anonymous funs in **different** functions
%%   named/0         the **named** lambda (`fun Fact/1`, recursive so the compiler
%%                   keeps the name); its generated name embeds the fun's own name
%%   external/0      an `EXPORT_EXT` (`fun lists:reverse/1`) fun, whose identity is
%%                   inlined in the term and never consults the seam
%%   name_items/0    the `fun_info/2` items for one local fun, so the item shapes
%%                   ride along with the triple
%%   info_agreement/0  the same fun's items **and** its `fun_info_mfa/1` triple, so
%%                   the two BIFs cannot drift apart
%%   mfa_item/0      `erlang:fun_info(F, mfa)` — `badarg` on the oracle (the C's
%%                   `switch (what)` has no `mfa` arm; the triple is
%%                   `fun_info_mfa/1`'s job)
-module(funmfa).
-export([single/0, siblings/0, spread/0, named/0, external/0,
         name_items/0, info_agreement/0, mfa_item/0]).

single() ->
    erlang:fun_info_mfa(fun(Y) -> Y + 1 end).

siblings() ->
    A = fun(X) -> X end,
    B = fun(Y) -> Y * 2 end,
    [erlang:fun_info_mfa(A), erlang:fun_info_mfa(B)].

spread() ->
    [erlang:fun_info_mfa(first()), erlang:fun_info_mfa(second())].

first() -> fun(X) -> X end.

second() -> fun(Y) -> Y end.

named() ->
    F = fun Fact(0) -> 1; Fact(N) -> N * Fact(N - 1) end,
    erlang:fun_info_mfa(F).

external() ->
    erlang:fun_info_mfa(fun lists:reverse/1).

name_items() ->
    F = fun(Y) -> Y + 1 end,
    {erlang:fun_info(F, name),
     erlang:fun_info(F, type),
     erlang:fun_info(F, module),
     erlang:fun_info(F, arity)}.

info_agreement() ->
    F = fun(Y) -> Y + 1 end,
    {erlang:fun_info(F, name),
     erlang:fun_info(F, module),
     erlang:fun_info(F, arity),
     erlang:fun_info(F, type),
     erlang:fun_info_mfa(F)}.

mfa_item() ->
    try erlang:fun_info(fun(Y) -> Y end, mfa) of
        Value -> {value, Value}
    catch
        error:badarg -> badarg
    end.
