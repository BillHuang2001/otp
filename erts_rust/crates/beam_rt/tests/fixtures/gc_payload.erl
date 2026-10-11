-module(gc_payload).

%% The runtime-shaped GC workload: a long-lived payload plus short-lived churn.
%%
%% `run(N)` builds a 64-element payload, performs `N` rounds of short-lived
%% allocation while keeping the payload live, and finally returns the payload
%% itself. Each round consults the payload through `erlang:length/1` *after* the
%% round's allocation, so the compiler must keep the payload a root (an X or Y
%% register, both of which are inside the Erlang stack the collector relocates)
%% across the round's heap-admission point. A runtime whose collector relocated
%% the heap against an incomplete root set would return a corrupted payload, so
%% the suite checks every element of the returned term.

-export([run/1, payload_length/0]).

%% The payload's element count — the value `erlang:length/1` answers on it.
payload_length() -> 64.

run(N) when is_integer(N), N >= 0 ->
    Payload = build(payload_length()),
    _ = keep(Payload, N, 0),
    Payload.

%% The long-lived payload: `[{K, K + 1, [K, K + 2, K + 3]} | ...]` for
%% `K = 64, 63, ..., 1`, so element `i` (0-based) has the key `64 - i`.
build(0) -> [];
build(K) -> [{K, K + 1, [K, K + 2, K + 3]} | build(K - 1)].

%% One round: allocate a 64-element chunk of short-lived terms (marked live to the
%% next instruction so the allocation cannot be elided), then read the payload so
%% it stays a root across that allocation.
keep(_Payload, 0, Acc) -> Acc;
keep(Payload, N, Acc) ->
    Garbage = churn(64),
    _ = erlang:length(Garbage),
    keep(Payload, N - 1, Acc + erlang:length(Payload)).

%% The short-lived garbage: a fresh 64-element list of small tuples per round.
churn(0) -> [];
churn(K) -> [{K, K + 1, [K, K + 2]} | churn(K - 1)].
