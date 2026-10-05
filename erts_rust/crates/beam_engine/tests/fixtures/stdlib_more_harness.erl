%% The second stdlib-harness fixture, for `tests/stdlib_more.rs`.
%%
%% Like `stdlists_harness`, every export is a thin wrapper around a *real system
%% stdlib* function, so running the wrapper through the engine proves the widened
%% typed-IR interpreter drives the actual stdlib `.beam` modules shipped with the
%% system Erlang (OTP 29 / `erts-17.1`, the differential oracle).
%%
%% This harness adds two kinds of coverage the first one does not:
%%
%% 1. `member/2` and `reverse/2` reach the **`lists:member/2` / `lists:reverse/2`
%%    NIFs** (whose `.beam` bodies are the `erlang:nif_error(undef)` stub), the
%%    one path those exports cannot be entered through directly.
%% 2. The remaining exports pass an **anonymous fun** into a real stdlib function
%%    (`lists:partition/2`/`splitwith/2`/`sort/2`/`foldl/3`/`zipwith/3`,
%%    `orddict:map/2`/`filter/2`/`update/3`/`fold/3`, `queue:filter/2`/`all/2`),
%%    so they exercise `make_fun3` + `call_fun*` + cross-module fun resolution
%%    into modules beyond `lists`.
%%
%% Compiled from a neutral directory (see `README.md`) so no worktree path is
%% baked into the committed `.beam`.
-module(stdlib_more_harness).

-export([
    member/2,
    reverse/2,
    lists_partition/1,
    lists_splitwith/1,
    lists_sort_desc/1,
    lists_foldl_sum/1,
    lists_zipwith_add/2,
    orddict_map/1,
    orddict_filter/1,
    orddict_update/1,
    orddict_fold/1,
    queue_filter_gt/1,
    queue_all_pos/1
]).

member(E, L) -> lists:member(E, L).

reverse(A, B) -> lists:reverse(A, B).

lists_partition(L) -> lists:partition(fun(X) -> X > 1 end, L).

lists_splitwith(L) -> lists:splitwith(fun(X) -> X < 3 end, L).

lists_sort_desc(L) -> lists:sort(fun(A, B) -> A >= B end, L).

lists_foldl_sum(L) -> lists:foldl(fun(X, Acc) -> X + Acc end, 0, L).

lists_zipwith_add(A, B) -> lists:zipwith(fun(X, Y) -> X + Y end, A, B).

orddict_map(D) -> orddict:map(fun(_K, V) -> V * 10 end, D).

orddict_filter(D) -> orddict:filter(fun(_K, V) -> V > 1 end, D).

orddict_update(D) -> orddict:update(a, fun(V) -> V + 1 end, D).

orddict_fold(D) -> orddict:fold(fun(_K, V, Acc) -> V + Acc end, 0, D).

queue_filter_gt(Q) -> queue:filter(fun(X) -> X > 2 end, Q).

queue_all_pos(Q) -> queue:all(fun(X) -> X > 0 end, Q).
