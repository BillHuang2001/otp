%% Receive/message opcode fixture for `tests/receive.rs`.
%%
%% Rebuilt with the system `erlc` (see tests/fixtures/README.md) and committed as
%% `recv.beam`. Every export is chosen so the compiler emits the receive-family
%% opcodes (`send`, `loop_rec`, `loop_rec_end`, `wait`, `wait_timeout`, `timeout`,
%% `remove_message`, `recv_marker_*`) as documented in the README table.
-module(recv).

-export([wait/0, bomb/1, sel/0, afterzero/0, scan/0, aftervar/1, marked/0]).

%% `receive X -> X end` — `loop_rec` + `remove_message` + `wait`.
wait() ->
    receive
        X -> X
    end.

%% `Pid ! 42` — the `send` opcode (destination in x(0), message in x(1)).
bomb(Pid) ->
    Pid ! 42,
    ok.

%% Selective receive: `foo` matches the literal clause, anything else is bound.
sel() ->
    receive
        foo -> a;
        Bar -> Bar
    end.

%% `receive _ -> got after 0 -> empty end` — the default clause matches the first
%% message; an empty mailbox reaches `timeout` directly (`after 0` is a literal
%% timeout the loader turns into a jump).
afterzero() ->
    receive
        _ -> got
    after 0 ->
        empty
    end.

%% A receive loop that scans past a non-matching head: `42` fails both clauses and
%% reaches `loop_rec_end`, then the next message is examined.
scan() ->
    receive
        foo -> foo;
        {bar, X} -> X
    end.

%% A timeout given as a *runtime* value: `T = 0` does not suspend (the
%% `wait_timeout` opcode falls through to `timeout`); `T > 0` suspends.
aftervar(T) ->
    receive
        foo -> foo
    after T ->
        empty
    end.

%% A reference created before the send is used in the receive, so `beam_ssa_recv`
%% inserts the marker prologue (`recv_marker_reserve` / `recv_marker_bind` /
%% `recv_marker_use` / `recv_marker_clear`).
marked() ->
    Ref = make_ref(),
    self() ! {Ref, 7},
    receive
        {Ref, Reply} -> Reply
    end.
