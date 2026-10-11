# stdlib — Erlang Standard Library

## Intent
The `stdlib` OTP application: the fundamental Erlang standard library that sits directly on top of `kernel`/`erts`.
It supplies the core data structures, text/Unicode handling, the `io` protocol, OTP behaviours (`gen_server` & friends), the Erlang source scanner/parser/linter/pretty-printer, archives/compression, and the Erlang shell.
Everything here ships with every OTP release; module names are effectively part of the language surface.

## API Surface
Build/release metadata:
- `Makefile` — `SUB_DIRECTORIES = src doc examples`, `VSN = $(STDLIB_VSN)`, `DIA_PLT_APPS=compiler crypto`, `TEST_NEEDS_RELEASE=true`; its `test` target re-invokes `make/app_targets.mk` with `ERL_ARGS="-enable-feature maybe_expr"`.
- `vsn.mk` — `STDLIB_VSN = 8.1` (substituted for `%VSN%` in the generated `stdlib.app`/`stdlib.appup`).
- `src/stdlib.app.src` — app resource: `{applications,[kernel]}`; `runtime_dependencies` is much wider (`sasl-3.0 kernel-11.0 erts-16.0.3 crypto-4.5 compiler-5.0 syntax_tools-3.2.1`) because those calls are optional/late-bound; registers `timer_server`, `rsh_starter`, `take_over_monitor`, `pool_master`, `dets`.
- `src/stdlib.appup.src` — release-upgrade instruction template (also `%VSN%`-substituted).
- `include/` — the six public headers other applications compile against: `assert.hrl`, `erl_bits.hrl`, `erl_compile.hrl`, `ms_transform.hrl`, `qlc.hrl`, `zip.hrl`.
- `examples/erl_id_trans.erl` is compiled into `ebin` by a rule in `src/Makefile` although its source lives outside `src/` and it is absent from `MODULES`/`stdlib.app`.

Module inventory, grouped as in `doc/docs.exs` (`groups_for_modules`): see `src/`.

## Constraints
- Build order: stdlib is in the first tier of `lib/Makefile`; the `primary_bootstrap_compiler` set (`epp`, `erl_anno`, `erl_error`, `erl_features`, `erl_scan`, `erl_parse`, `erl_lint`, `graph`, `io`, `otp_internal`, `erl_internal`) is compiled against the bootstrap compiler first — nothing else in OTP builds until these exist.
- Runtime dependency rule: `{applications,[kernel]}` only; stdlib must not hard-depend on `crypto`, `compiler`, `syntax_tools` or `sasl`.
- New warnings break the build: sources compile with `-Werror +warn_deprecated_catch +warn_obsolete_bool_op -I../include -I../../kernel/include`.
- Generated files: `src/erl_parse.erl` (yecc from `src/erl_parse.yrl`) and `src/unicode_util.erl` (from `uc_spec/`) are git-ignored build products; `src/otp_internal.erl` is checked in but auto-generated from `-deprecated`/`-removed` attributes tree-wide — regenerate it via `./otp_build update_deprecations` (`scripts/update_deprecations`), never edit it by hand.
- Adding or removing a module requires editing both `MODULES` in `src/Makefile` and `{modules,[...]}` in `src/stdlib.app.src` (no automatic sync).
- Public documentation lives in `-moduledoc`/`-doc` attributes in `src/`, not in `doc/`; internal modules must declare `-moduledoc false`.
- New public headers go in `include/` and must be added to `HRL_FILES` in `src/Makefile` to be installed; stdlib-private headers (`dets.hrl`, `erl_tar.hrl`, `otp_internal.hrl`, `json.hrl`, `swar_ascii.hrl`) stay in `src/`.
- `ebin/` is build output (only `.gitignore` tracked).

## Notes for Agents
- Deprecated-but-present modules: `dict`, `random` (→`rand`), `slave` (→`peer`), `gen_fsm` (→`gen_statem`), plus legacy `error_logger_file_h`/`error_logger_tty_h`; their entries are generated into `src/otp_internal.erl` and consumed by `erl_lint`.
- `zip.hrl` is a contract with `erts`: the preloaded `erts/preloaded/src/prim_zip.erl` is compiled against `lib/stdlib/include`, so the record shapes cannot change unilaterally.
- `assert.hrl` is the most widely included header in OTP (used by erts, compiler, kernel, dialyzer, common_test, …) and is documented in `doc/references/assert_hrl.md`; it is the only stdlib header with an include guard.
- Docs are built by the repo-root `make/doc.mk` + `make/ex_doc.exs`; `doc/docs.exs` is only a per-app override (module groups and extras order).
- `zstd` is a thin wrapper over the erts NIF (`erts/emulator/nifs/common/zstd_nif.c`); the codec lives in `erts/`.
- Compiler-front-end modules have a *pure* runtime surface: `erl_scan`, `erl_parse`, `erl_lint`, `io_lib`, `io_lib_format`, `io_lib_pretty` call only Erlang modules (erl_anno, epp, unicode, string, lists, maps, gb_sets/ordsets/sets, string) plus `erlang:*` BIFs — no `crypto`, `zlib`, `persistent_term` or `ets`. `beam_lib` is the exception: it calls `zlib` (LitT literals + gzip input/`strip`) and `crypto` (`application:start(crypto)` for encrypted `debug_info`), does direct `file`/`filelib`/`filename` I/O, and uses a private temp `ets` table while decoding atom chunks.
- `records` (native records, OTP 29+) and `json` (EEP-68, OTP 27+) are thin wrappers over runtime BIFs / recently added public modules.
- Property-based suites are skipped (green) when no QuickCheck/PropEr/Triq is installed — a passing run may not have exercised them.

## Dependencies
- Toolchain: `yecc` (for `erl_parse.yrl`), `escript` (for `unicode_util` generation and `scripts/update_deprecations`), ExDoc for docs.
- No external system libraries; `zstd`/`zlib` linkage is handled by `erts/`.

## Test Strategy
- `make test` must be run from `lib/stdlib` (not from `test/`); `TEST_NEEDS_RELEASE=true` makes it release an OTP into the test dir and run Common Test against it, with the `maybe_expr` feature enabled.
- Suites are `*_SUITE.erl` in `test/` plus a shared `property_test/` directory backing both dedicated `*_property_test_SUITE`s and the property groups of other suites.
- Test selection specs next to the suites: `stdlib.spec` (default: all suites, benchmark groups skipped), `stdlib_bench.spec` (benchmark-only groups), `stdlib_gh.spec` (CI variant, selected via `SPEC_POSTFIX=gh`), `error_info.spec` (the `erl_error`/lint error-info groups), and the `stdlib.cover`/`tty.cover` coverage specs.
- System- and network-dependent cases are excluded via spec skips (`ets_SUITE:partly_bound`, `gen_server_SUITE:multicall_down`, peer/dist groups).

## Routing Table
- `src/` → the Erlang modules themselves (collections, text, io, behaviours, code/parsing, archives, shell) plus `stdlib.app.src`/`stdlib.appup.src` and the internal headers
- `test/` → Common Test suites, property tests, suite data dirs, helper modules and test specs
- `doc/` → ExDoc configuration (`docs.exs`), guides, references, assets and per-module `.md` doc sources
- `include/` → public headers exported to other applications (`assert.hrl`, `qlc.hrl`, `ms_transform.hrl`, `zip.hrl`, …)
- `examples/` → illustrative modules; `erl_id_trans` is built into `ebin` and documented, `my_error_module` is not compiled
- `scripts/` → maintenance scripts (`update_deprecations`, which regenerates `src/otp_internal.erl`)
- `uc_spec/` → vendored Unicode Character Database inputs + generator for `src/unicode_util.erl`
- `ebin/` → build output only (git-ignored) — do not document

## See Also
- `../kernel/` — the only declared application dependency; supplies the primitives stdlib builds on.
- `../../erts/` — supplies the BIFs/NIFs used by `ets`, `binary`, `records`, `zstd` and the preloaded modules (`prim_zip` compiles against `include/zip.hrl`).
- `../compiler/` — consumes `erl_scan`/`erl_parse`/`erl_lint`/`erl_pp` from here.
- `../tools/`, `../common_test/` — tooling that exercises stdlib.
