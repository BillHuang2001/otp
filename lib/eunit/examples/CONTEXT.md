# eunit — Examples

## Intent
Small worked examples of writing EUnit tests, shipped as documentation material and also used as test input by the EUnit self-tests.

## API Surface
- `fib.erl` — sample module with a test-generating function `fib_test_/0` containing 8 assertions (`?assert`, `?assertException`); it has no `-export` for the tests themselves (they are auto-exported by `eunit_autoexport`).
- `tests.txt` — a one-line EUnit test-description file (`[eunit_lib].`), i.e. an Erlang-term file consumed via `eunit:test/1` on a *filename*.
- `Makefile` — no build targets of its own; `release_docs_spec` installs `fib.erl` and `tests.txt` into `$(RELEASE_PATH)/lib/eunit-$(VSN)/doc/examples`.

## Constraints
- Content is user-facing documentation: keep it minimal and idiomatic.
- `fib.erl` is compiled by the test suite (`../test/eunit_SUITE.erl` builds it into this directory and runs `{dir, ...}` over it) — the assertion count (8) is asserted there, so changing it requires updating `../test/eunit_SUITE.erl`.
- `tests.txt` is referenced by a relative path from the `-ifdef(TEST)` self-tests in `../src/eunit_data.erl`, so those tests require cwd = the app root.
- This directory is copied into the released test directory by `../test/Makefile`, not into the code release.

## See Also
- `../src/eunit_data.erl` — parses both `fib_test_/0`-style generators and `tests.txt` files; `../test/eunit_SUITE.erl` — exercises these examples.
