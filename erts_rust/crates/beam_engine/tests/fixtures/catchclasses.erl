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
%%
%% `thrown/0`/`exited/0` raise through `erlang:raise/3` — the class-carrying BIF
%% the compiler lowers the `throw(X)` sugar to, and the only raise BIF with an
%% explicit class the test host's BIF floor serves. The class-`error` shapes use
%% the in-VM `badmatch` opcode instead, so their reason and stacktrace come from
%% the engine's own raise path exactly as a compiled `case`/`=` failure does.
-module(catchclasses).
-compile(nowarn_deprecated_catch).
-export([plain/0, thrown/0, exited/0, errored/0, nested/0]).

plain() -> catch ok.
thrown() -> catch erlang:raise(throw, ball, []).
exited() -> catch erlang:raise(exit, bye, []).
errored() -> catch (a = b).
nested() -> catch deeper().
deeper() -> a = b.
