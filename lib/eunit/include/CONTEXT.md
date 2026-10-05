# eunit — Public Header

## Intent
Holds the single public header of the `eunit` application, `eunit.hrl`, which is what user code includes (`-include_lib("eunit/include/eunit.hrl")`).
Including it is the *only* supported way to get EUnit test macros and the automatic test export/strip behaviour.

## API Surface
`eunit.hrl` (single file, guard `?EUNIT_HRL`) provides:
- Compile switches read from the including module's environment: `TEST`/`NOTEST`, `DEBUG`/`NODEBUG`, `ASSERT`/`NOASSERT`, `EUNIT_NOAUTO`, `EUNIT_DEBUG_VAL_DEPTH`.
  Including the header defines `TEST` and `EUNIT` unless `NOTEST` is defined; `ASSERT`/`NOASSERT` are forwarded to stdlib's `assert.hrl`.
- `-include_lib("stdlib/include/assert.hrl")` — the `?assert*`/`?assertEqual`/`?assertMatch`/`?assertException`/... macros themselves come from **stdlib**, not from EUnit; eunit only wraps them.
- Parse-transform directives: `-compile({parse_transform, eunit_autoexport})` when tests are enabled, else `-compile({parse_transform, eunit_striptests})`; both suppressed by `EUNIT_NOAUTO`.
- Test-creating ("underscore") macros returning `{Line, fun}` test objects: `?_test/1` plus `?_assert`, `?_assertNot`, `?_assertMatch`, `?_assertNotMatch`, `?_assertEqual`, `?_assertNotEqual`, `?_assertException`, `?_assertError`, `?_assertExit`, `?_assertThrow`, `?_assertNotException`.
- Utility macros: `?LET/3`, `?IF/3`, `?MATCHES/2`, `?UNDER_EUNIT`, `?capturedOutput` (the latter two consult `eunit_proc` at runtime).
- External-command macros: `?_cmd_/1`, `?cmd/1`, `?_cmd/1`, `?cmdStatus/2`, `?_cmdStatus/2`, `?assertCmd*`/`?_assertCmd*` (delegate to `eunit_lib:command/1`).
- Debugging/instrumentation macros: `?randomDelay/3`, `?debugMsg/1`, `?debugHere`, `?debugFmt/2`, `?debugVal/1,2`, `?debugValAll/1,2`, `?debugTime/2`.

## Constraints
- The header is public API for arbitrary (also non-OTP) code: keep macro names, arities and semantics stable, and keep them usable with testing/complexity switched off (`NOTEST`).
- Macro bodies must not leak variable bindings into the caller: they use `(fun (...) -> ... end)()` wrappers and `__`-prefixed variables, and must never introduce bindings that clash with user code.
- No new runtime dependency may be pulled in via the header — macros must not require EUnit present at runtime except the documented `?UNDER_EUNIT`/`?capturedOutput`/`?cmd*` cases.
- License note kept in the file: object files using these macros are explicitly *not* considered derived works under the LGPL (`Apache-2.0 OR LGPL-2.1-or-later`).
- Adding/removing/renaming a macro obliges a matching update of the macro overview in `../doc/guides/chapter.md`.

## Notes for Agents
- `EUNIT_NOAUTO` is the escape hatch for code that must not depend on the eunit parse transforms at compile time; surrounding test code with `-ifdef(EUNIT)` is the documented portable alternative.
- `?UNDER_EUNIT` checks the *group leader's* `current_function` (`eunit_proc`), so it does not work in secondary processes with a redirected group leader.

## See Also
- `../src/eunit_autoexport.erl`, `../src/eunit_striptests.erl` — the transforms the directives select (suffixes `_test`, `_test_`, `_exported_`, overridable via compiler options).
- `../src/eunit_proc.erl` — implements `?UNDER_EUNIT` / `?capturedOutput` support; `../src/eunit_lib.erl` — implements the command macros.
- `../doc/guides/chapter.md` — user documentation of every macro.
