# eunit — Lightweight Unit-Testing Framework

## Intent
The `eunit` OTP application: a lightweight unit-testing framework for Erlang (`eunit:test/1,2` plus the `?assert*`-style macros in `include/eunit.hrl`).
It is a *pure library* application (kernel + stdlib only) whose purpose is to be included and called by other code, both inside OTP and by external projects.
Version is `EUNIT_VSN = 2.11` (`vsn.mk`); it is built only in a full build (it is in `ALL_ERLANG_APPLICATIONS`, not in the first-built `ERTS_APPLICATIONS`/`ERLANG_APPLICATIONS` lists in `../Makefile`).

## API Surface
- `eunit` module — the user entry point: `start/0`, `stop/0`, `test/1`, `test/2`, plus an "experimental; may be removed or relocated" block (`test/3`, `submit/1..3`, `start/1`, `stop/1`, `watch*`/`watch_path*`/`watch_regexp*`/`watch_app*`).
- `eunit_surefire` module — documented listener producing Maven/Bamboo-style Surefire XML, selected via the `{report, {eunit_surefire, [{dir, Dir}]}}` test option.
- `include/eunit.hrl` — the public macro API (`?_test`, `?_assert*`, `?LET`, `?IF`, `?MATCHES`, `?UNDER_EUNIT`, `?capturedOutput`, `?cmd*`, `?debug*`); including it turns testing on and auto-exports test functions.
- Auto-exported test naming contract (see `src/eunit_internal.hrl`): `*_test/0` simple tests, `*_test_/0` generators, `*_exported_/0` explicit exports, and a generated `test/0` that calls `eunit:test(Module)`; `Module_tests` is run alongside `Module` unless `{exact_execution, true}`.
- Test-representation grammar accepted by `test/1,2` (defined as the `tests()` type in `src/eunit_data.erl`): modules, `{module, M}`, `{application, A[, Info]}`, filenames/`{file, F}`, `{dir, D}`, `{generator, ...}`, `{setup, ...}`, `{foreach, ...}`, `{foreachx, ...}`, `{node, ...}`, `{timeout, ...}`, `{spawn, ...}`, `{inorder, ...}`, `{inparallel[, N], ...}`, `{with, X, Fs}`, titles `{String, Tests}` and deep/improper lists.
- Custom listeners plug in through `{report, {Module, Options}}`; `eunit_listener` defines the callback behaviour (`init/1`, `handle_begin/3`, `handle_end/3`, `handle_cancel/3`, `terminate/2`).
- Build entries: `Makefile` (`SUB_DIRECTORIES = src examples doc`, `VSN = $(EUNIT_VSN)`), `src/Makefile` (compiles into `../ebin`, substitutes `%VSN%` in `eunit.app.src`/`eunit.appup.src`), `test/Makefile` (Common Test suite, released as `$(RELEASE_PATH)/eunit_test`), `doc/Makefile` (`make/doc.mk`).
- Consumed elsewhere by `include_lib("eunit/include/eunit.hrl")` (e.g. `erts/emulator/test/*_SUITE.erl`, `lib/syntax_tools/`, `lib/tools/test/cover_SUITE_data/`), and external editors (Emacs `lib/tools/emacs/erlang-eunit.el` shells out to `eunit:test/1`).

## Dependencies
- Runtime: `kernel`, `stdlib` only (`{applications, [kernel,stdlib]}`, `runtime_dependencies: stdlib-6.0, kernel-11.0, erts-9.0` in `src/eunit.app.src`) — do not add `tools`/`compiler` or any other runtime dependency.
- Assert macros are provided by `stdlib/include/assert.hrl`, which `include/eunit.hrl` includes.
- Tests additionally require `common_test` (self-suite) and `xmerl`/`unicode` (Surefire XML validation in the suite); the OTP 30 test suite is run through `ts`.

## Constraints
- `src/` is compiled with `-Werror` plus `+warn_deprecated_catch +warn_obsolete_bool_op +warn_unused_import`; any warning breaks the build.
- Everything in `ebin/` (`../ebin` for `src/`, `test/` compiles in place) and the generated `eunit.app`/`eunit.appup` are build artifacts — never edit or commit them.
- App version changes happen in `vsn.mk` only (`EUNIT_VSN`); the release-wide version mapping is the repo-root `otp_versions.table`.
- The `eunit.hrl` macro names/semantics and the `tests()` grammar are public API: changes need matching documentation updates in `doc/guides/chapter.md` and in the `-doc` attributes of `src/eunit.erl`.
- `eunit_autoexport` is a compile-time dependency of every user module that includes `eunit.hrl` (unless `EUNIT_NOAUTO`/`-ifdef(EUNIT)` is used) — its suffix contract cannot change casually.

## Design Decisions
- Tests are *values*, not functions: a test is a fun or a data structure (generator) describing tests, which is why `eunit_data` only needs a lazy iterator and test sets compose freely.
- The named-module contract keeps test code out of the way: test functions need no `-export` because a parse transform (`eunit_autoexport`) exports them, and a matching transform (`eunit_striptests`) removes them when testing is off.
- Automatic stripping/exporting is deliberately optional (`EUNIT_NOAUTO`) because it makes even a non-test build depend on this header and transform.
- Results are delivered through a listener protocol (`eunit_serial` orders events, `eunit_listener` counts, `eunit_tty` is the default reporter) so alternate reporters like `eunit_surefire` are plug-ins rather than special cases.
- The framework depends only on kernel/stdlib and is meant to be usable as a library without an OTP release, so all external-process support lives in `eunit_lib:command/1..3` behind macros.

## Known Issues
- The module-watch feature (`watch*`, `watch_app`) is inert: targets are registered but the `code_monitor` producer of `{code_monitor, {loaded, ...}}` messages does not exist in the tree — see `src/CONTEXT.md`.
- EUnit's own self-tests reference relative paths (`examples/tests.txt`, `./ebin/*.beam`), so they must be run with cwd = `lib/eunit` (the CT suite does `file:set_cwd(code:lib_dir(eunit))`).
- `test/eunit_SUITE.erl` mutates the process-wide cwd without restoring it, so its cases are not parallel-safe.

## Notes for Agents
- Most of the codebase lives in `src/`; `src/eunit_data.erl` (test representation), `src/eunit_proc.erl` (process tree/output capture), `src/eunit_serial.erl` (event ordering) and `src/eunit_server.erl` (server/watches) are the core.
- `include/eunit.hrl` defines `TEST` unless `NOTEST` is set, so the `-ifdef(TEST)` self-tests inside `src/*.erl` are compiled in ordinary builds.
- The release layout is split: code (`ebin`/`src`/`include`), tests (`$(RELEASE_PATH)/eunit_test`, via `test/Makefile`) and docs/examples (`doc/`, `doc/examples`) — `src/Makefile`, `test/Makefile` and `examples/Makefile` own those parts.
- `doc/notes.md` are release notes (newest entry `Eunit 2.11`) and are appended per release, not rewritten.

## Test Strategy
- EUnit is tested by itself from three layers: in-module `-ifdef(TEST)` suites inside `src/eunit_proc.erl`/`eunit_test.erl`/`eunit_data.erl`, external self-tests in `src/eunit_tests.erl`, and the Common Test suite in `test/eunit_SUITE.erl` (15 cases covering primitives, fixtures, `{node, ...}`, Surefire XML/encodings, `scale_timeouts` and `parse_command_line/2`).
- Run the app suite with `make test` in `lib/eunit` (or `ts:run(eunit, eunit, [batch])` from the OTP `test_server`); a single case via `make ARGS="-suite eunit_SUITE -case ..." test`.
- `test/eunit.cover` instruments the whole `eunit` app except `eunit_test`.

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | All 13 Erlang modules: public `eunit`/`eunit_surefire`, internals (`eunit_data`, `eunit_proc`, `eunit_serial`, `eunit_server`, `eunit_test`, `eunit_lib`, `eunit_listener`, `eunit_tty`), parse transforms (`eunit_autoexport`, `eunit_striptests`), self-tests (`eunit_tests`), plus `eunit_internal.hrl`, `eunit.app.src`, `eunit.appup.src` |
| `include/` | `eunit.hrl` — the public macro/switch API and the parse-transform directives |
| `test/` | Common Test self-suite `eunit_SUITE.erl`, custom listener `eunit_test_listener.erl`, dummy test modules (`tc0`, `tlatin`, `tutf8`, `ttimesout`), `eunit.spec`/`eunit.cover` |
| `doc/` | ExDoc config (`docs.exs`), user guide (`guides/chapter.md`), release notes (`notes.md`), doc `Makefile` |
| `examples/` | `fib.erl` and `tests.txt` worked examples (released as doc examples and used as test input) |
| `ebin/` | Build output only (`.gitignore` tracked) — ignore |

## See Also
- `../CONTEXT.md` — how apps in `lib/` are built and released; `../common_test/` — the framework used by `test/eunit_SUITE.erl`; `../tools/` — Emacs `erlang-eunit.el` integration.
- `../../HOWTO/TESTING.md` (repo root `HOWTO/`) — how to run an app's test suites.
