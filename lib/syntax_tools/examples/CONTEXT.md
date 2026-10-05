# syntax_tools — Example Programs

## Intent
Runnable example/tutorial programs shipped with the `syntax_tools` application (`syntax_tools-4.1.1`, see `../vsn.mk`).
They are documentation assets, not part of the OTP build: only `demo.erl` is installed, into `$(RELEASE_PATH)/lib/syntax_tools-$(VSN)/doc/examples` (rule `release_docs_spec`).

## API Surface
- `demo.erl` — self-instructing tour: `run/0` loads the sibling modules and prints a prompt for each step, `run_1/0`–`run_4/0` walk through pretty-printing with decreasing paper width, `view/1,2,3` display the pretty-printed form of a file.
- `demo` internals: `read/1` = `epp:parse_file` + `erl_comment_scan:file` + `erl_recomment:recomment_forms`; `print/1,2,3` = `erl_prettypr:format` with `{paper,_}`/`{ribbon,_}` options.
- `test.erl` — tiny annotated module used as `demo:small_file()`; a demo input, not a demo module.
- `test_comprehensions.erl` — `demo:medium_file()`; exercises binary and list comprehensions (requires `-compile(binary_comprehension)`).
- `demo:big_file()` is the literal string `"erl_comment_scan.erl"`, i.e. `../src/erl_comment_scan.erl` of this app (no path is resolved — see Constraints).
- `Makefile` — build glue only: includes `make/target.mk`, `make/$(TARGET)/otp.mk`, `../vsn.mk`, defines `RELSYSDIR`/`EXAMPLE_FILES = demo.erl`, and has empty `$(TYPES)`, `clean`, `docs` rules.
- `merl/` — Merl metaprogramming tutorial and its own standalone Makefile; see `merl/CONTEXT.md`.

## Constraints
- Nothing here is compiled by the OTP build (`$(TYPES):` is an empty rule and `make/otp_subdir.mk` recurses into this dir only for release targets); compile and run the examples by hand.
- `demo.erl` is also documentation content: `../doc/demo.erl` is a symlink to it and `../doc/Makefile` copies it to the generated `doc/assets/demo.erl`, so it must stay valid parseable source (it is parsed, not compiled, by the doc build).
- `demo.erl` carries `-compile([{nowarn_possibly_unsafe_function, {erlang, list_to_atom, 1}}])`.
- The demo file names are relative to the current working directory: run from `src/` (or copy the inputs there) for `run_1..run_4` to find `test.erl`, `test_comprehensions.erl` and `erl_comment_scan.erl`.
- Keep the example modules free of dependencies beyond the `syntax_tools` app itself (plus `epp`/`erl_prettypr`), so a plain `erlc` in a built checkout suffices.

## Notes for Agents
- To try the demo: `erlc demo.erl` in this directory, start `erl -pa ../ebin` from `../src`, then `demo:run()` and follow the printed prompts (`demo:run/0` calls `make:all([load])`).
- This Makefile never recurses into `merl/`; the Merl examples are driven only by `merl/Makefile`.

## Dependencies
- `syntax_tools` itself (`erl_syntax`, `erl_prettypr`, `erl_recomment`, `erl_comment_scan`) must be compiled and on the code path (`../ebin`).
- `epp` (stdlib) for parsing and the `make` module (tools) for `make:all/1`; no external packages.

## Test Strategy
- No test suite here; `../test/` holds the app's Common Test suites and `merl/` runs its own EUnit tests via its Makefile.

## See Also
- `../` — syntax_tools app context; `../src/` for the modules demo exercises; `../doc/` for how `demo.erl` is embedded in the docs; `merl/` for the Merl examples.
