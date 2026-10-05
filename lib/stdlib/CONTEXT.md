# stdlib — Erlang Standard Library

## Intent
The `stdlib` OTP application: the fundamental Erlang standard library that sits directly on top of `kernel`/`erts`.
It supplies the core data structures, text/Unicode handling, `io` protocol, OTP behaviours (`gen_server` & friends), the Erlang source scanner/parser/linter/pretty-printer, archives/compression, and the Erlang shell.
Everything in this directory is shipped with every OTP release; module names are effectively part of the language surface.

## API Surface
Build/release metadata:
- `Makefile` — `SUB_DIRECTORIES = src doc examples`, `VSN = $(STDLIB_VSN)`, `DIA_PLT_APPS=compiler crypto`, `TEST_NEEDS_RELEASE=true`; its `test` target re-invokes `make/app_targets.mk` with `ERL_ARGS="-enable-feature maybe_expr"`.
- `vsn.mk` — `STDLIB_VSN = 8.1` (the app version substituted for `%VSN%` in the generated `stdlib.app`/`stdlib.appup`).
- `src/stdlib.app.src` — app resource: `{applications,[kernel]}`; `runtime_dependencies` is much wider (`sasl-3.0 kernel-11.0 erts-16.0.3 crypto-4.5 compiler-5.0 syntax_tools-3.2.1`) because some modules call into those apps optionally; declares `registered` processes (`timer_server`, `dets`, `pool_master`, `rsh_starter`, `take_over_monitor`).
- `src/stdlib.appup.src` — release-upgrade instructions template (also `%VSN%`-substituted).
- `include/` — the public headers other applications compile against: `assert.hrl`, `erl_bits.hrl`, `erl_compile.hrl`, `ms_transform.hrl`, `qlc.hrl`, `zip.hrl`.

Module inventory (grouped as in `doc/docs.exs`): see `src/`.

## Constraints
- Build order: stdlib is built immediately after `stdlib sasl kernel compiler` in `lib/Makefile`'s first tier; the modules listed in `src/Makefile`'s `primary_bootstrap_compiler` (`epp`, `erl_anno`, `erl_error`, `erl_features`, `erl_scan`, `erl_parse`, `erl_lint`, `graph`, `io`, `otp_internal`, `erl_internal`) are compiled against the **bootstrap** compiler into `$(BOOTSTRAP_COMPILER)` first — the rest of OTP cannot be compiled until these exist.
- Runtime dependency rule: `stdlib.app.src` declares only `kernel`; stdlib must not hard-depend on `crypto`, `compiler`, `syntax_tools` or `sasl`. Where those are referenced (e.g. `error_logger` domain tags, `erl_compile` drivers, `re` fallbacks) the calls are optional/late-bound.
- Sources are compiled with `-Werror +warn_deprecated_catch +warn_obsolete_bool_op` and `-I../include -I../../kernel/include` — new warnings break the build.
- Never edit generated files: `src/erl_parse.erl` (from `src/erl_parse.yrl` via yecc) and `src/unicode_util.erl` (from `uc_spec/` via `uc_spec/gen_unicode_mod.escript`) are produced by the build.
- `ebin/` is build output (only `.gitignore` tracked); editing Erlang docs happens in `-moduledoc`/`-doc` attributes in `src/`, not in `doc/`.
- New public headers go in `include/` and must be added to `HRL_FILES` in `src/Makefile` for release packaging.

## Notes for Agents
- Deprecated-but-present modules still in `src/`: `dict`, `random` (→`rand`), `slave` (→`peer`), `gen_fsm` (→`gen_statem`), plus deprecated functions in `lists`, `queue`, `sys`, `calendar`, `gen_server`/`gen_event`/`gen_statem` callbacks.
- Doc generation is ExDoc-based (`doc/docs.exs` defines module groups in `groups_for_modules`); only `ms_transform`, `peer`, `qlc`, `re`, `shell`, `sofs` have separate `.md` doc sources — everything else documents itself from source attributes.
- `zstd` is a thin API wrapper over an erts NIF (`erts/emulator/nifs/common/zstd_nif.c`); the actual codec lives in `erts/`, not here.
- `records` (native records) is a wrapper over runtime BIFs (OTP 29+).

## Dependencies
- Toolchain: `yecc` (for `erl_parse.yrl`), `escript` (for `unicode_util` generation), ExDoc for docs.
- No external system libraries; `zstd`/`zlib` linkage is handled by `erts/`.

## Test Strategy
- `make test` in this directory (Common Test), running against an installed release (`TEST_NEEDS_RELEASE=true`) with `maybe_expr` enabled.
- Suites are `*_SUITE.erl` in `test/` plus a `property_test/` directory of PropEr/QuickCheck-style property modules used by the `*_property_test_SUITE` suites.
- Test selection specs live beside the suites: `stdlib.spec`, `stdlib_bench.spec` (benchmark-only groups), `stdlib_gh.spec`, `suite_release.exclude` (skips `tar_SUITE` in release tests), `error_info.spec`.

## Routing Table
- `src/` → the Erlang modules themselves (collections, text, io, behaviours, code/parsing, archives, shell) plus `stdlib.app.src`/`stdlib.appup.src`
- `test/` → Common Test suites, property tests, suite data dirs and test specs
- `doc/` → ExDoc configuration (`docs.exs`), guides, references and per-module `.md` doc sources
- `include/` → public headers exported to other applications (`assert.hrl`, `qlc.hrl`, `ms_transform.hrl`, `zip.hrl`, …)
- `examples/` → sample code compiled/built as part of the app (`erl_id_trans`, `my_error_module`)
- `scripts/` → maintenance scripts (`update_deprecations`)
- `uc_spec/` → Unicode Character Database input files + generator for `src/unicode_util.erl`
- `ebin/` → build output only (git-ignored) — do not document

## See Also
- `../kernel/` — the only declared application dependency; kernel provides the primitives stdlib builds on.
- `../../erts/` — supplies the BIFs/NIFs used by `ets`, `binary`, `records`, `zstd` and the preloaded modules.
- `../compiler/` — consumes `erl_scan`/`erl_parse`/`erl_lint`/`erl_pp` from here.
- `../tools/`, `../common_test/` — test/release tooling that exercises stdlib.
