-module(gc_class_copy_bound).
%% The **C3 (messaging / copy-bound)** workload class of the GC-strategy A/B harness
%% (`../gc_strategy_classes.rs`): two shapes in one module, both driven by a real spawned
%% process and both returning a payload the Rust side checks element by element.
%%
%% * `send_recv(N)` — a genuine two-process ping-pong. The run's process spawns a peer, sends it
%%   a fresh payload, receives it back (both directions of the process copy boundary are
%%   exercised), validates the round trip with a bound-variable pattern (`case T2 of T -> …`,
%%   which is a real equality test that raises on a mismatch), *joins* the peer through a monitor
%%   before returning, and returns the last round-tripped payload. Every `receive` is
%%   `after`-bounded, so a lost message is a loud exit instead of a hang.
%% * `ets_copy(N)` — the ETS copy boundary: one table and one key, a fresh payload inserted and
%%   copied back out with `ets:lookup/2` per round (so the work is the copy, never table growth),
%%   the looked-up term validated with the same bound-variable pattern, and the last copy
%%   returned.
%%
%% The payload is `{K, [{K, I, I + K} || I <- 1..64]}` — 64 4-word tuples behind 64 cons cells,
%% ~390 words, comfortably above the default 233-word nursery, so a round's allocation asks a
%% heap-admission door for more than a whole nursery and the collector really runs. The payload
%% is rebuilt every round (that allocation is the class's pressure) and dies at the next round.
-export([send_recv/1, ets_copy/1]).

-define(PAYLOAD_LEN, 64).
-define(RECV_TIMEOUT_MS, 60000).

send_recv(N) when is_integer(N), N > 0 ->
    Peer = spawn(fun() -> peer() end),
    Monitor = erlang:monitor(process, Peer),
    Last = send_recv(N, Peer, none),
    Peer ! stop,
    receive
        {'DOWN', Monitor, process, Peer, _Reason} -> ok
    after ?RECV_TIMEOUT_MS ->
        exit(join_timeout)
    end,
    Last.

send_recv(0, _Peer, Last) ->
    Last;
send_recv(N, Peer, _Last) ->
    T = payload(N),
    Peer ! {self(), T},
    receive
        {Peer, T2} ->
            case T2 of
                T -> send_recv(N - 1, Peer, T2)
            end
    after ?RECV_TIMEOUT_MS ->
        exit({no_reply, N})
    end.

peer() ->
    receive
        {From, T} ->
            From ! {self(), T},
            peer();
        stop ->
            ok
    end.

ets_copy(N) when is_integer(N), N > 0 ->
    Table = ets:new(gc_class_copy_bound, [set, public]),
    Last = ets_copy(N, Table, none),
    ets:delete(Table),
    Last.

ets_copy(0, _Table, Last) ->
    Last;
ets_copy(N, Table, _Last) ->
    T = payload(N),
    true = ets:insert(Table, {payload, T}),
    case ets:lookup(Table, payload) of
        [{payload, T2}] ->
            case T2 of
                T -> ets_copy(N - 1, Table, T2)
            end
    end.

payload(K) ->
    {K, [{K, I, I + K} || I <- lists:seq(1, ?PAYLOAD_LEN)]}.
