-module(stacktrace_frames).
-export([t0/0, t2/0, t3/0]).
t0() -> try f() catch _C:_R:S -> S end.
f() -> erlang:error(boom).


hh() -> erlang:error(boom).
t2() -> try hh() catch _C:_R:S -> S end.
t3() -> try g2() catch _C:_R:S -> S end.
g2() -> hh2() + 1.
hh2() -> erlang:error(boom).

%% Cooked-catch-stacktrace oracle fixture (see ../stacktrace_frames.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic stacktrace_frames.erl`.
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
