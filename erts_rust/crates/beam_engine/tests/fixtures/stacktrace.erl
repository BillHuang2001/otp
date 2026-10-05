%% Stacktrace / raw-raise oracle fixture (see ../exceptions.rs).
%%
%% Compiled with the system toolchain: `erlc stacktrace.erl`.
%%
%% The source stays inside the opcode subset the Milestone-1 interpreter
%% executes, and deliberately pins the two `try ... catch C:R:S` handler
%% shapes the compiler can emit:
%%
%%   * `cap/1`     — the cooked form: the handler stores C and R, moves the raw
%%                   stacktrace from x(2) into x(0) and runs `build_stacktrace`.
%%   * `reraise/1` — `catch C:R:S -> erlang:raise(C, R, S)`, which
%%                   `sys_core_fold:opt_build_stacktrace/1` folds into the
%%                   `raw_raise` instruction (there is no need to cook a
%%                   stacktrace that is only re-raised).
%%   * `badclass/1`— the same shape with a *literal* class that is none of
%%                   `error`/`exit`/`throw`, so `raw_raise` takes its
%%                   "unknown class" path: x(0) is set to `badarg` and control
%%                   falls through to the compiler's own `move badarg, x(0)`.
%%   * `classify/1`— the plain `error:`-classed form of `cap/1`; it also interns
%%                   the `error` and `case_clause` atoms, which the engine
%%                   resolves from the module atom table when `case_end` raises
%%                   and a landing installs the exception-class atom (a bare
%%                   `C:R:S` pattern leaves both out of the atom table).
-module(stacktrace).
-export([cap/1, reraise/1, badclass/1, classify/1]).

cap(X) -> try case X of 1 -> one end catch C:R:S -> {C,R,S} end.

reraise(X) -> try case X of 1 -> one end catch C:R:S -> erlang:raise(C, R, S) end.

badclass(X) -> try case X of 1 -> one end catch _C:R:S -> erlang:raise(nonsense, R, S) end.

classify(X) -> try case X of 1 -> one end catch error:{case_clause,_} -> caught_case_clause end.
