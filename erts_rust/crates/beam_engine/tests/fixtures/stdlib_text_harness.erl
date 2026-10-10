%% The text/encoding harness fixture, for `tests/stdlib_text.rs`.
%%
%% Like `stdlists_harness` and `stdlib_more_harness`, every export is a thin
%% wrapper around a *real system stdlib* function, so running the wrapper through
%% the engine proves the widened typed-IR interpreter drives the actual stdlib
%% `.beam` modules shipped with the system Erlang (OTP 29 / `erts-17.1`, the
%% differential oracle).
%%
%% The exports here reach the **fun-taking** exports of `lists` and `ordsets`
%% (`any/2`, `all/2`, `foldr/3`, `mapfoldl/3`, `search/2`, `zf/2`,
%% `ordsets:filter/2`, `ordsets:fold/3`) — exports that cannot be entered
%% directly because the caller has to build an anonymous fun, so they exercise
%% `make_fun3` + `call_fun*` + cross-module fun resolution into `lists`/`ordsets`.
%%
%% Compiled from a neutral directory with `erlc +deterministic` (see `README.md`)
%% so no worktree path is baked into the committed `.beam`.
-module(stdlib_text_harness).

-export([
    lists_any_gt/1,
    lists_all_pos/1,
    lists_foldr_sub/1,
    lists_mapfoldl_add/1,
    lists_search_gt/1,
    lists_zf_double/1,
    ordsets_filter_gt/1,
    ordsets_fold_rev/1
]).

lists_any_gt(L) -> lists:any(fun(X) -> X > 1 end, L).

lists_all_pos(L) -> lists:all(fun(X) -> X > 0 end, L).

lists_foldr_sub(L) -> lists:foldr(fun(X, Acc) -> X - Acc end, 0, L).

lists_mapfoldl_add(L) -> lists:mapfoldl(fun(X, Acc) -> {X + Acc, X} end, 0, L).

lists_search_gt(L) -> lists:search(fun(X) -> X > 2 end, L).

lists_zf_double(L) -> lists:zf(fun(X) -> {true, X * 2} end, L).

ordsets_filter_gt(S) -> ordsets:filter(fun(X) -> X > 1 end, S).

ordsets_fold_rev(S) -> ordsets:fold(fun(X, Acc) -> [X | Acc] end, [], S).
