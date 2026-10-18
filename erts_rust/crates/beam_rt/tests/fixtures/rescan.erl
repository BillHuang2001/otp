%% Selective-receive scan-position (marker) fixture for `tests/mailbox_rescan.rs`.
%%
%% Rebuilt with the system `erlc` and committed as `rescan.beam` (see
%% tests/fixtures/README.md for the regeneration recipe). Every export drives the
%% receive/marker family — `recv_marker_reserve`, `recv_marker_bind`,
%% `recv_marker_use`, `wait`, `wait_timeout`, `timeout`, `remove_message` — through
%% the *real* runtime glue, so a run proves the mailbox scan position is a live
%% per-process quantity and not a per-call one.
%%
%% The oracle values are Python/erl-verified and listed in tests/fixtures/README.md.
-module(rescan).

-export([parked/0, marked/0, junk_then_ref/0, two_receives/0, timed/0, timedhit/0]).%% Oracle: `rescan:parked()` -> `42`.
%%
%% The receiver parks on an EMPTY mailbox (`loop_rec` finds no candidate, `wait`
%% suspends) and a *later* message — sent by another process — is what it takes.
parked() ->
    Self = self(),
    spawn(fun() -> Self ! 42 end),
    receive
        X -> X
    end.

%% Oracle: `rescan:marked()` -> `done`.
%%
%% The **marker'd** form: the receive matches on a reference created just before
%% it, which is the shape `beam_ssa_recv` rewrites into
%% `recv_marker_reserve`/`recv_marker_bind`/`recv_marker_use` (the compiler proves
%% no message queued before the `make_ref/0` can match). The mailbox is empty when
%% the marker is reserved, so this is the empty-anchor case end to end: the marker
%% must still be usable once the spawned sender's message arrives.
marked() ->
    Self = self(),
    Ref = make_ref(),
    spawn(fun() -> Self ! {Ref, done} end),
    receive
        {Ref, X} -> X
    end.

%% Oracle: `rescan:junk_then_ref()` -> `7`.
%%
%% A non-matching message is queued *before* the reference exists, so the marker
%% anchors after it: the selective receive must skip it and take the message queued
%% afterwards.
junk_then_ref() ->
    self() ! junk,
    Ref = make_ref(),
    self() ! {Ref, 7},
    receive
        {Ref, X} -> X
    end.

%% Oracle: `rescan:two_receives()` -> `3`.
%%
%% Two receives sharing one reference: each starts with `recv_marker_use`, and the
%% first one **removes** the message it took, so the second one's use runs against a
%% queue that shrank by one in front of the anchor.
two_receives() ->
    Self = self(),
    self() ! junk,
    Ref = make_ref(),
    Self ! {Ref, 1},
    Self ! {Ref, 2},
    A = receive
            {Ref, X} -> X
        end,
    B = receive
            {Ref, Y} -> Y
        end,
    A + B.

%% Oracle: `rescan:timed()` -> `timeout` (after 20 ms).
%%
%% An empty mailbox, nothing that can match, and a finite deadline: the scan must
%% run out and the `after` clause must fire.
timed() ->
    Ref = make_ref(),
    receive
        {Ref, X} -> {got, X}
    after 20 -> timeout
    end.

%% Oracle: `rescan:timedhit()` -> `done` (the deadline does **not** fire).
%%
%% The same finite deadline, but a message that matches arrives while the receiver
%% is parked: the timeout must be cancelled, not raced.
timedhit() ->
    Self = self(),
    Ref = make_ref(),
    spawn(fun() -> Self ! {Ref, done} end),
    receive
        {Ref, X} -> X
    after 20 -> timeout
    end.
