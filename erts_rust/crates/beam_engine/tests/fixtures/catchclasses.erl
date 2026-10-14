%% Catch-class oracle fixture (see ../exceptions.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic catchclasses.erl`.
%%
%% The three exception classes the old-style `catch` folds at `catch_end`
%% (C `emu/instrs.tab:1188-1211` plus `beam_common.c:730-735`):
%%   * `thrown/0` — class `throw`: the caught term is the thrown value BARE.
%%   * `exited/0` — class `exit`: the caught term is the bare `{'EXIT',Reason}`,
%%                  with NO stacktrace element.
%%   * `errored/0` — class `error`: the caught term is `{'EXIT',{Reason,Stacktrace}}`
%%                   (C's `add_stacktrace`).
%%   * `nested/0` — the same class-`error` shape raised through a NON-tail local
%%                  call, so the cooked stacktrace names more than one frame.
%%   * `plain/0` — no exception at all: the value passes through unchanged.
-module(catchclasses).
-export([plain/0, thrown/0, exited/0, errored/0, nested/0]).

plain() -> catch ok.
thrown() -> catch throw(ball).
exited() -> catch exit(bye).
errored() -> catch erlang:error(boom).
nested() -> catch deeper().
deeper() -> erlang:error(boom).
