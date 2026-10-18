%% Exception/control-flow variant-proof fixture for
%% `tests/real_beam_variant_proofs/exceptions.rs` (the real-`.beam` proof suite).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic vb_exc.erl`.
%%
%% Every export exists to make the installed compiler emit one exception-family
%% instruction, and to keep the result comparable with the oracle (`~w` of a
%% returned value, or of `{Class,Reason}` from a wrapping `try ... catch`):
%%
%%   * `badmatch/1`   — `badmatch`        (`ok = X`)
%%   * `case_end/1`   — `case_end`        (a `case` with no matching clause)
%%   * `if_end/1`     — `if_end`          (an `if` with no true guard)
%%   * `rethrow/1`    — `raise`           (a `try ... catch` whose clauses do not
%%                                          match: the compiler's fallback clause
%%                                          re-raises the pending exception)
%%   * `rethrow_after/1` — `raise`        (`try ... after`: the after-path
%%                                          re-raises after running the body)
%%   * `recover/1`    — `try_case`        (the exceptional handler entry, catching)
%%   * `try_clause/1` — `try_case_end`    (a `try ... of` value matching no clause)
%%   * `normal_try/1` — `try_end`         (the normal exit of a `try ... of`)
%%   * `old_catch/1`  — `catch_end`       (old-style `catch`, folding the class)
%%
%% `throw(X)` is not used directly: the compiler lowers it to
%% `call_ext erlang:raise/3` (`catchclasses.erl` documents the same choice), which
%% the test host's BIF floor serves with its class and reason intact.
-module(vb_exc).
-compile(nowarn_deprecated_catch).
-export([badmatch/1, case_end/1, if_end/1, rethrow/1, rethrow_caught/1,
         rethrow_after/1, recover/1, try_clause/1, normal_try/1, old_catch/1]).

%% `badmatch` — `error:{badmatch, X}`.
badmatch(X) -> ok = X, matched.

%% `case_end` — `error:{case_clause, X}`.
case_end(X) -> case X of 1 -> one end.

%% `if_end` — `error:if_clause`.
if_end(X) -> if X > 0 -> positive end.

%% `raise` — the no-catch-clause-matched re-raise (the exception escapes with its
%% original class and reason).
rethrow(X) -> try bump(X) catch throw:nope -> caught end.

%% The same re-raise, landed in an OUTER `try` so the row also observes a value.
rethrow_caught(X) -> try rethrow(X) catch error:badarith -> reraised end.

%% `raise` from a `try ... after` (the exception is re-raised after the body).
rethrow_after(X) -> try bump(X) after ok end.

%% `try_case` — the exceptional path of a `try ... catch`, which returns a value.
recover(X) -> try bump(X) catch error:badarith -> recovered end.

%% `try_case_end` — a `try ... of` value that matches no `of` clause. The body
%% cannot raise, so the compiler drops the `try` frame and the `try_clause` raise
%% is uncaught: `error:{try_clause, X}`.
try_clause(X) -> try X of 1 -> one catch error:{try_clause,V} -> {caught,V} end.

%% `try_end` — the normal exit of a `try ... of` (`{ok, X + 1}`).
normal_try(X) -> try bump(X) of V -> {ok,V} catch error:_ -> failed end.

%% `catch_end` — old-style `catch`; a class-`throw` exception folds to the thrown
%% value BARE (`ball`).
old_catch(X) -> catch erlang:raise(throw, X, []).

%% The raising worker: `+` on a non-number raises `badarith`.
bump(X) -> X + 1.
