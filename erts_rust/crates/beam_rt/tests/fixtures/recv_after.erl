%% Finite `receive … after T` fixture for `beam_rt`'s scheduler tests.
%%
%% Rebuilt with the system `erlc` and committed as `recv_after.beam` (the same
%% recipe as the sibling fixtures):
%%
%%     erlc -o tests/fixtures tests/fixtures/recv_after.erl
%%
%% Every export is chosen for the exact opcode shape the runtime reads:
%%
%% * `after20/0` / `after2000/0` — a **literal** positive timeout compiles to
%%   `Fail: wait_timeout ScanLabel, {integer,T}` followed by `timeout`, which is
%%   the shape `runner::parked_receive_timeout` recovers the timed-out path from;
%% * `afterzero/0` — a literal `0` timeout is folded away by the compiler (no
%%   `wait_timeout` at all), so the empty mailbox jumps straight to `timeout`;
%% * `self_send/0` — a message already in the mailbox, so the receive matches it
%%   and never arms a deadline;
%% * `wait_forever/0` — `after infinity` also compiles to a bare `wait`.
-module(recv_after).

-export([after20/0, after2000/0, afterzero/0, self_send/0, wait_forever/0]).

%% `receive X -> X after 20 -> timeout end` — parks ~20 ms, then times out.
after20() ->
    receive
        X -> X
    after 20 ->
        timeout
    end.

%% The same with a delay long enough that a message (or no answer at all) is the
%% interesting case.
after2000() ->
    receive
        X -> X
    after 2000 ->
        timeout
    end.

%% `after 0` never suspends: the compiler emits no `wait_timeout` and an empty
%% mailbox falls through to `timeout`.
afterzero() ->
    receive
        X -> X
    after 0 ->
        timeout
    end.

%% A message already in the mailbox when the receive starts: it is matched and the
%% (long) `after` clause is never reached.
self_send() ->
    self() ! hello,
    receive
        X -> X
    after 2000 ->
        timeout
    end.

%% `after infinity` parks indefinitely (the compiler emits a bare `wait`).
wait_forever() ->
    receive
        X -> X
    after infinity ->
        timeout
    end.
