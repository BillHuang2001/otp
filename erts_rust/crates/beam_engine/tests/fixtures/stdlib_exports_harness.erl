%% The third stdlib-harness fixture, for `tests/stdlib_exports_more.rs`.
%%
%% Like `stdlists_harness` / `stdlib_more_harness` / `stdlib_text_harness`, every
%% export is a thin wrapper around a *real system stdlib* function, so running the
%% wrapper through the engine proves the widened typed-IR interpreter drives the
%% actual stdlib `.beam` modules shipped with the system Erlang (OTP 29 /
%% `erts-17.1`, the differential oracle).
%%
%% The exports here reach three things a direct call cannot:
%%
%% 1. `lists:keysearch/3` — a `lists` **NIF** (its `.beam` body is the
%%    `erlang:nif_error/undef` stub), so it is only enterable through a caller's
%%    `call_ext`, which the host's BIF floor serves.
%% 2. `ordsets:map/2` / `ordsets:filtermap/2` — the last two fun-taking `ordsets`
%%    exports, which need an anonymous fun built by the caller (`make_fun3` +
%%    `call_fun*` + cross-module fun resolution into `ordsets`/`lists`).
%%
%% Compiled from a neutral directory with `erlc +deterministic` (see `README.md`)
%% so no worktree path is baked into the committed `.beam`.
-module(stdlib_exports_harness).

-export([
    lists_keysearch/3,
    ordsets_map_double/1,
    ordsets_filtermap_double/1
]).

lists_keysearch(Key, N, TupleList) -> lists:keysearch(Key, N, TupleList).

ordsets_map_double(S) -> ordsets:map(fun(E) -> E * 2 end, S).

ordsets_filtermap_double(S) -> ordsets:filtermap(fun(E) -> {true, E * 2} end, S).
