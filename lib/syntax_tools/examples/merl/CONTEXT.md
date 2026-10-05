# Merl Examples — Interpreter/Compiler Pairs

## Intent
Tutorial and benchmark set for Merl (`../src/merl.erl`), the compile-time metaprogramming support in `syntax_tools`.
Each sample language is implemented twice — as a plain Erlang interpreter and as a Merl-based compiler that generates, compiles and loads Erlang code — so the two can be compared and share one test file.

## API Surface
- `basic.erl` (`basic:run/2`) — Basic interpreter; program is a list of `{Line, Stmt}` pairs held in `ets` tables (`var`, `line`), statements `print/set/goto/stop/iff`.
- `basicc.erl` (`basicc:run/2`, `make_lines/1`, `bool/1`) — the same Basic language compiled to Erlang with Merl `?Q` quasiquotes, one generated function per source line plus `run/stop/goto`, assembled with `merl_build` and loaded via `merl:compile_and_load/2` as module `tmp`; generated code calls `basicc:make_lines/1` and `basicc:bool/1`.
- `lisp.erl` (`lisp:eval/1`, plus `init/0`, `equal/2`, `gt/2`, `knot/1` kept exported for the compiler) — Lisp interpreter with closure/dict environments, a Y combinator and builtins (`print`, `list`, `apply`, `plus`, `equal`, `gt`, `knot`).
- `lispc.erl` (`lispc:eval/1`) — Lisp compiled to Erlang; generated `tmp` module closes over `fun`s and calls back into `lisp:equal/2`, `lisp:gt/2`, `lisp:knot/1`.
- `merl_build.erl` — small `#module{}` builder used by both compilers: `init_module/1`, `module_forms/1` (produces the `-module/-export/-import/-record/-file/function` form list for `merl:compile_and_load/2`), `add_function/4`, `add_record/3`, `add_import/3`, `add_attribute/3`, `set_file/2`.
- `basic_test.erl`, `lisp_test.erl` — EUnit tests that are `-include`d, not standalone modules (the `-module` lines are commented out); the shared test generators call the host module's `run/2` or `eval/1` unqualified, so each file must be included into a module exporting that function.
- `Makefile` — standalone driver (see below); not reachable from the OTP build.

## Constraints
- Naming convention: `X.erl` is the interpreter, `Xc.erl` is the compiled counterpart of the same language; `Xc.erl` reuses `X_test.erl` so both variants are covered by identical expectations.
- `-define(INTERPRETED, true)` before `-include("lisp_test.erl")` selects interpreter-only tests (`interpreter_basics_test_/0`, e.g. `{undefined, foo}` throws and `{builtin,_}` values); `lisp.erl` defines it, `lispc.erl` deliberately does not. `basic_test.erl` has no such guard.
- Any module including `merl.hrl` gets the `merl_transform` parse transform (unless `MERL_NO_TRANSFORM`); the header lives at `../../include/merl.hrl` and needs `../../ebin/merl.beam` and `../../ebin/merl_transform.beam` on the code path — this is why the Makefile passes `-pa ../../ebin` and declares those two beams as prerequisites of `basicc.beam`/`lispc.beam`.
- `basic.erl` intentionally includes only `eunit.hrl` and `basic_test.erl` (no `merl.hrl`) — the interpreter must need no parse transform.
- Both compilers always generate a module named `basicc:tmp` / `lispc:tmp` and load it into the running VM; the EUnit tests therefore cannot be run in parallel, and the generated module reuses the `var`/`line` `ets` tables.
- Files here are not built or installed by the OTP build: the Makefile uses relative `../../ebin`, `../../include` and plain `erlc`/`erl`, so it must be run from inside this directory in a tree where `syntax_tools` has already been compiled (it does not use `ERL_TOP`).

## Notes for Agents
- `make all` (the default target) compiles the five sources and then runs `erlc`-free EUnit over `[lisp, lispc, basic, basicc]` via `erl -noshell -pa ../../ebin`; `make clean` removes the `.beam` files.
- The `test` target invokes the system `erl`, not a locally built `erts/`, so it needs a usable OTP installation plus the compiled `../../ebin`.
- `merl_build.erl` and the `?Q`-based compilers are the reference examples of `merl:compile_and_load/2`; commented-out `file:write_file(...erl_prettypr:format...)` blocks in `basicc.erl`/`lispc.erl` dump the generated source for debugging.

## Dependencies
- Merl runtime from this same app (`merl`, `merl_transform`) and its header `../../include/merl.hrl`.
- EUnit (`eunit/include/eunit.hrl`, part of `eunit`) for the `*_test.erl` files.
- `erl_syntax`/`erl_prettypr` (used by `merl_build`) — same application.

## Test Strategy
- Tests live inline in `basic_test.erl`/`lisp_test.erl` and are executed through the host modules (`eunit:test([lisp, lispc, basic, basicc], [])`), including a Fibonacci benchmark (`fib_test_/0`) comparing the interpreter against the generated code.
- The app's formal suites are `../test/merl_SUITE.erl` and `../test/syntax_tools_SUITE.erl`; this directory is illustrative only and is not run by CI.

## See Also
- `../` — the syntax_tools example programs (`demo.erl`) and the release/doc rules; `../../src/merl.erl` and `../../include/merl.hrl` for the API these examples use.
