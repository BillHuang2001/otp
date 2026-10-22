-module(stacktrace_frames).
-export([t0/0, t2/0, t3/0, withcall/0, inhandler/0, inhandler2/0, park/0, folded/0, refold/0]).
t0() -> try f() catch _C:_R:S -> S end.
f() -> erlang:error(boom).


hh() -> erlang:error(boom).
t2() -> try hh() catch _C:_R:S -> S end.
t3() -> try g2() catch _C:_R:S -> S end.
g2() -> hh2() + 1.
hh2() -> erlang:error(boom).

parent(ok) -> ok.
withcall() ->
    parent(ok),
    try 1 / 0 catch _C:_R:S -> S end.
g(X) -> 1 / X.
inhandler() ->
    try g(0) catch _:_ -> 1 / 0 end.
park() ->
    parent(ok),
    receive _ -> ok end.
folded() ->
    try 1 / 0 catch C:R:S -> erlang:raise(C, R, S) end.
refold() ->
    try g(0) catch C:R:S -> erlang:raise(C, R, S) end.
inhandler2() ->
    try g(0) catch _:_ -> try 1 / 0 catch _C:_R:S -> S end end.
%% Cooked-catch-stacktrace oracle fixture (see ../stacktrace_frames.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen2" erlc +deterministic stacktrace_frames.erl`
%% (so the `Line` chunk's file name is the bare `stacktrace_frames.erl`, never an
%% absolute path).
%%
%% The three `try ... catch _C:_R:S -> S end` shapes that pin DEFECT 2 (a reported
%% "duplicated top stacktrace frame" that does NOT reproduce — C's
%% `gather_stacktrace` records each continuation pointer once):
%%
%%   * `t0/0` — the try body calls `f/0`, which raises `erlang:error(boom)`
%%              directly (a non-tail local call), so the cooked `S` runs `f/0`
%%              first and its live caller `t0/0` second, each EXACTLY ONCE.
%%   * `t2/0` — the same shape as `t0/0` with different names (`hh/0`), the
%%              control that a second identical chain does not double a frame.
%%   * `t3/0` — the try body TAIL-calls `g2/0`, which calls `hh2/0` non-tail
%%              (`hh2() + 1`). The oracle ELIDES `g2/0` from the cooked `S`
%%              (the tail call reuses `t3/0`'s frame), leaving `hh2/0` then
%%              `t3/0`.
%%
%% The four rows that pin DEFECT 1 (a frame's continuation-pointer slot must be
%% cleared by whoever follows it — C's `RETURN()` does `*E = NIL`,
%% `erts/emulator/beam/emu/macros.tab:241-247`), so a caller that already
%% RETURNED is never named again:
%%
%%   * `withcall/0` — `parent(ok)` returns, then the try body raises (`1 / 0`).
%%                    The frame is named ONCE, at the raising `fdiv`; a stale
%%                    slot would name it a second time at the returned-from call.
%%   * `inhandler2/0` — the try body's non-tail `g(0)` raises and LANDS in this
%%                    frame's catch handler (consuming the frame's slot); the
%%                    handler's OWN nested `try 1 / 0 catch _C:_R:S -> S end` then
%%                    cooks `S` naming the frame exactly ONCE, at line 28. A stale
%%                    slot would add a `g/1` frame.
%%   * `folded/0` — `sys_core_fold` rewrites `catch C:R:S -> erlang:raise(C,R,S)`
%%                    into the `raw_raise` instruction, which keeps the trace the
%%                    re-raised exception already carried (C's `raw_raise` clears
%%                    `EXF_SAVETRACE`, `beam_common.c:1148-1179`) — so `S` is the
%%                    ORIGINAL raise chain, with no re-raise-site frame.
%%   * `refold/0` — the same fold over a try whose body made a NON-TAIL `g(0)`
%%                    call, so the re-raised exception's chain has two frames:
%%                    `g/1` (the raising `fdiv`) then `refold/0` (its caller) — no
%%                    re-raise-site frame, and never `refold/0` twice.
%%
%% Two shapes are present but NOT asserted:
%%
%%   * `inhandler/0` — its handler re-raises `1 / 0` and that raise leaves the
%%                    module uncaught, so it reaches the harness as the fieldless
%%                    `BeamError::BadArith`; a fieldless reason carries no symbolic
%%                    frames by design (`beam_common::Raise`'s variants have no
%%                    `StackTrace` slot), so there is nothing to compare. The
%%                    landing clear it exercises is pinned instead by
%%                    `inhandler2/0`, whose nested catch cooks an `S`.
%%   * `park/0` — the test `ModuleHost` supplies no mailbox, so the `receive`
%%                    reports `unsupported_opcode: receive_next` before the machine
%%                    can park.