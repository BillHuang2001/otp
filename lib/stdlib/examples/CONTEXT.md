# stdlib examples

## Intent
Illustrative Erlang modules that ship as part of the `stdlib` application and back the standard-library documentation.
Unlike `../test/`, these are not throwaway samples: `erl_id_trans` is the canonical identity parse transform that `compile`/`epp` documentation points users to.
The directory is only three files — `erl_id_trans.erl`, `my_error_module.erl`, `Makefile`.

## API Surface
- `erl_id_trans.erl` (769 lines) — identity parse transform.
  - `parse_transform/2` (via `forms/1`, `form/1`, `expr/1`, `pattern/1`, `type/1`, …) walks the input forms and returns them unchanged.
  - `parse_transform_info/0` returns `#{error_location => column}` (documented contract: the compiler keeps column numbers only if the transform does not claim this).
  - Because it handles every legal abstract form (attributes, expressions, patterns, guards, types, comprehensions, maps), it doubles as a complete reference for `erl_parse:abstract_form()`; new Erlang syntax must be added here to keep the transform an identity.
  - Its `-moduledoc` explains `{parse_transform,Module}` and the `parse_transform_info/0` protocol.
- `my_error_module.erl` — the `format_error/2` + `error_info` example: `atom_to_string/1` raises with `{error_info, #{module => ?MODULE, cause => ...}}` and `format_error/2` returns the map with `general`/`reason` keys.
  Its text is duplicated verbatim in the `-moduledoc` of `../src/erl_error.erl` (the `c(my_error_module)` shell transcript example).
- `Makefile` — `$(TYPES)` (opt/debug/...), `clean` and `docs` are deliberately EMPTY rules (they must exist so `make/<TYPES>` recursion from `../Makefile` does not fail); it only defines release rules.

## Build & Release Behaviour
- `examples` is a `SUB_DIRECTORIES` entry of `../Makefile` (`SUB_DIRECTORIES = src doc examples`), so `make opt|clean|docs` recurse here, but nothing is compiled in this directory.
- `erl_id_trans.beam` is compiled by `../src/Makefile` (`ERL_ID_TRANS=../examples/erl_id_trans.erl`, rule `$(EBIN)/erl_id_trans.beam: ../examples/erl_id_trans.erl`, reached via `opt: $(EBIN)/erl_id_trans.beam`) into `../ebin`, using the app's `ERL_COMPILE_FLAGS`.
- That beam in `../ebin` is what ExDoc documents: `erl_id_trans` is listed in the `CODE` group of `../doc/docs.exs`, and doc build happens in `../doc/` from the compiled beams.
- `release_docs_spec` installs `EXAMPLES` (= `erl_id_trans.erl` only) to `$(RELEASE_PATH)/lib/stdlib-$(STDLIB_VSN)/doc/examples`.
- `release_tests_spec` tars this whole directory into `$(RELEASE_PATH)/stdlib_test/examples`.

## Constraints
- Compiled with the app's flags (`-Werror` from `../src/Makefile`, plus `+warn_missing_doc_function`/`+warn_missing_doc_callback`/`+warn_missing_spec_documented` from `make/otp.mk.in`): every exported function of `erl_id_trans` needs matching `-doc`/`-spec` or the build fails.
- `erl_id_trans` is NOT in the `MODULES` list of `../src/Makefile`, so it is not part of `stdlib.app`'s module list — only compiled into `ebin` for docs and tests.
- Keep `my_error_module.erl` in sync with the code block embedded in `../src/erl_error.erl`'s moduledoc.
- Do not expect new `.erl` files added here to be compiled automatically; only `erl_id_trans.erl` has an explicit build rule.
- Renaming/moving `erl_id_trans.erl` breaks `../test/id_transform_SUITE.erl`, which finds it by the literal path `<dirname of installed suite>/examples/erl_id_trans.erl`.

## Notes for Agents
- `my_error_module.erl` is neither compiled by the build nor installed by `release_docs_spec` (only `erl_id_trans.erl` is listed in `EXAMPLES`); it is the source counterpart of the `erl_error` documentation example and is referenced nowhere else in the repo.
- `id_transform_SUITE` only asserts that the transform output equals its input — it is an API smoke test, not a semantic test of the transform.

## Test Strategy
- `../test/id_transform_SUITE.erl` (suite `id_transform`) compiles the installed copy of `erl_id_trans.erl`, loads it, applies `parse_transform/2` to the abstract code of every beam on the code path and in sibling `*_test` dirs, and fails unless the result is byte-identical to the input; it is skipped under valgrind (too slow).
- Because the suite reads the file from the release install, examples must be installed by `release_tests_spec` for the suite to pass (`TEST_NEEDS_RELEASE=true` for stdlib).

## See Also
- `../src/Makefile` — compiles `erl_id_trans.beam` into `../ebin`.
- `../src/erl_error.erl` — moduledoc example mirrored by `my_error_module.erl`.
- `../doc/docs.exs` — `CODE` group listing `erl_id_trans`; docs generated from `../ebin/erl_id_trans.beam`.
- `../../compiler/src/compile.erl` — the authoritative `parse_transform`/`parse_transform_info` documentation, pointing at `erl_id_trans`.
- `../test/id_transform_SUITE.erl` — the consumer/consumer test of this directory.
