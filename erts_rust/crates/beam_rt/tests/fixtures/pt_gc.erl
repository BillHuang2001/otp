-module(pt_gc).

%% The persistent-term-across-a-collection workload.
%%
%% `run(N)` builds a 64-element payload on the process's own **young heap**,
%% stores it in the node-global persistent-term table, churns `N` rounds of
%% short-lived garbage (filling the young heap and driving collections at the
%% engine's heap-admission point) and finally returns
%% `persistent_term:get(pt_gc_key)` — the value read back OUT of the store.
%%
%% The payload's element count comes through `payload_length/0` (a function
%% call the compiler cannot fold), so `build/1` really allocates at run time
%% rather than returning a literal. A store that kept the raw word from the
%% (movable) young heap would hand back a dangling/recycled term here, which the
%% suite's element-by-element payload check catches.

-export([run/1, payload_length/0]).

%% The payload's element count — the value `erlang:length/1` answers on it.
payload_length() -> 64.

run(N) when is_integer(N), N >= 0 ->
    Payload = build(payload_length()),
    ok = persistent_term:put(pt_gc_key, Payload),
    _ = keep(N, 0),
    persistent_term:get(pt_gc_key).

%% The payload: `[{K, K + 1, [K, K + 2, K + 3]} | ...]` for
%% `K = 64, 63, ..., 1`, so element `i` (0-based) has the key `64 - i`.
build(0) -> [];
build(K) -> [{K, K + 1, [K, K + 2, K + 3]} | build(K - 1)].

%% One round: allocate a fresh 64-element chunk of short-lived terms (marked
%% live to the next instruction so the allocation cannot be elided), then drop
%% it. The churn is the allocation pressure that fills the young heap.
keep(0, Acc) -> Acc;
keep(N, Acc) ->
    Garbage = churn(64),
    _ = erlang:length(Garbage),
    keep(N - 1, Acc + 1).

%% The short-lived garbage: a fresh 64-element list of small tuples per round.
churn(0) -> [];
churn(K) -> [{K, K + 1, [K, K + 2]} | churn(K - 1)].
