# erts/test/ — ERTS system-level (released-installation) test suites

## Intent
Common Test suites that exercise Erlang/OTP **as installed and released**, i.e. the runtime system from the outside: the launcher scripts (`erl`, `erlexec`, `erlc`, `run_erl`), the install layout, release upgrade, whole-release consistency, and the C support libraries (`ethread`, `erl_printf`).
Distinct from the two neighbouring ERTS test areas: `../emulator/test` (VM internals — terms, GC, tracing, NIF/driver/port semantics, built against emulator internals) and `../epmd/test` (`epmd_SUITE`).
Because these suites need a real released tree they are the ERTS "system tests": `erts/Makefile`'s `test` target sets `TEST_NEEDS_RELEASE=true` and delegates to `../../make/test_target_script.sh`.

## API Surface

### Suites (`MODULES` in `Makefile`, installed to `$(RELEASE_PATH)/system_test`)
| Suite | Covers | Fixtures |
|---|---|---|
| `otp_SUITE` | Whole-release consistency via `xref` from an installed release: undefined functions, `deprecated`/`obsolete` consistency, calls to deprecated functions, `call_to_size_1`/`call_to_now_0`, `erl`/`xml` file encodings, `runtime_dependencies_*` and `test_runtime_dependencies_versions` (app-version × OTP-release matrix) | `otp_SUITE_data/` (empty in git; filled at release time) |
| `install_SUITE` | `make/install_bin` install/symlink logic: `DESTDIR`, `EXTRA_PREFIX`, `exec_prefix`, `bindir` variants, relative/absolute `ln -s`, unreachable dirs, missing `dirname`, whitespace in paths | `install_SUITE_data/dirname`, `install_SUITE_data/install_bin` (generated) |
| `erlc_SUITE` | The `erlc` driver: `.erl`/`.yrl`/`.mib`/`.script` compilation, `-make` dependency output, `deep_cwd`, arg overflow, unicode paths, `features_*` (feature flags), run with and without the `erlc` **server** (`ERLC_USE_SERVER` yes/no/features groups) | `erlc_SUITE_data/{src,include}` |
| `erlexec_SUITE` | `erlexec` (the `erl` launcher) command-line handling: `-args_file` (incl. evil/missing/unicode/env-substituted variants), env handling, argument separation, too-large arguments, `zdbbl` dist buffer limit | `erlexec_SUITE_data` (C argv-dumper) |
| `run_erl_SUITE` | `run_erl`/`to_erl` pipe logging, heavy/heavier load, reaping of defunct children | `run_erl_SUITE_data` (Perl) |
| `upgrade_SUITE` | `release_handler` upgrades of the running release: `minor` (X → X_patched), `major` (X-1 → X), `ancient_major` (X-2 → X) | `upgrade_SUITE_data/start.src` |
| `ethread_SUITE` | C-level `ethread` abstraction: mutex/`try_lock`/cond+`broadcast`, `tsd`, spin/rw locks, atomics, detached threads, `max_threads`, thread names. Linked against `erts_internal_r` (static erts libs) | `ethread_SUITE_data` (C) |
| `erl_print_SUITE` | C-level `erl_printf`/`snprintf`/quoting: integer, float, string, character, `snprintf`, `quote`, `erlang_display`. Built twice (thread-safe and non-thread-safe) | `erl_print_SUITE_data` (C + `*_test.h` format tables) |
| `nt_SUITE` | Windows NT/win32 only: `erlsrv` services (start/env/priority/logout/debug/restart/stopaction), event-log handling, process info | `nt_SUITE_data` (C, uses `@erl_include@`) |
| `z_SUITE` | Must run **last**: scans the build/test tree for leftover core files (`core_files`, `search_for_core_files/1`); uses `OTP_DAILY_BUILD_TOP_DIR` and `$ERL_TOP/bin/cerl` | — |
| `parallel_messages_SUITE` | Throughput benchmark of parallel signal/message sending and `message_queue_data` switching (`off_heap`), incl. `visualize_throughput.html` report; also exports `throughput_benchmark/0`, `large_throughput_benchmark/0` for manual runs | `parallel_messages_SUITE_data/visualize_throughput.html` |

`ERTS_MODULES = erts_test_utils` is **compiled from `$(ERL_TOP)/erts/emulator/test/erts_test_utils.erl`** (cross-directory dependency; the `.erl` is copied into the release). `erl_print_SUITE` is its only consumer here (`mk_ext_pid/3`, `mk_ext_port/2`, `mk_ext_ref/2`).

Support files (not suites): `ignore_cores.erl` (see Known Issues), `utils/gccifier.c` + `utils/gccifier.sh` (compiler wrapper that echoes the command line; used by the data-dir `Makefile.src` files, shell copy used when cross-compiling).

### Spec files
- `system.spec` — the default spec, literally `{suites,"../system_test",all}.` (the path is relative to the spec's own directory, i.e. the installed `system_test` suite dir).
- `system_smoke.spec` — fast subset: `{cases,...,otp_SUITE,[undefined_functions, deprecated_not_in_obsolete, obsolete_but_not_deprecated, call_to_size_1, call_to_now_0]}` plus `{skip_cases,...,ethread_SUITE,[max_threads],"Skip"}`.
- `system.dynspec` — conditional fragment that skips `ethread_SUITE` and `erl_print_SUITE` when the test C compiler differs from the one OTP's static libs were built with. Header comment documents evaluation via `file:script("system.dynspec", [{'TestCCompiler',{msc|gnuc, undefined}}])`.
- Spec selection: `make/test_target_script.sh` prefers `system_$(SPEC_POSTFIX).spec` over `system.spec` when it exists — so `SPEC_POSTFIX=smoke make test` selects `system_smoke.spec`. CI (`.github/workflows/reusable-*.yaml`) passes `SPEC_POSTFIX=gh` (no `system_gh.spec` exists, so it falls back to `system.spec`).

## Constraints
- Requires a built **and released** OTP tree; the suites run from `$(RELEASE_PATH)/system_test` against the release in `$RELEASE_PATH` (`RELSYSDIR = $(RELEASE_PATH)/system_test`). A bare `make`-built tree is not enough.
- `RELSYSDIR` receives the spec files, `$(ERL_FILES)` (both `*_SUITE.erl` and `erts_test_utils.erl`), the compiled `*.beam`, and verbatim tarballs of `*_SUITE_data` and `utils/` (same relative layout — data-dir `Makefile.src` references `..@DS@utils@DS@gccifier.*`, so the layout must stay flat).
- `release_tests_spec` copies `$(ERL_TOP)/otp_versions.table` and `$(ERL_TOP)/make/otp_version_tickets` into `otp_SUITE_data`; the released tests must therefore be self-contained. Both files are generated — `otp_SUITE_data/.gitignore` is the only tracked file in that directory.
- `install_SUITE_data/install_bin` is generated by copying `../../make/install_bin` (`EXTRA_FILES`), not tracked, removed by `make clean`.
- C fixtures in `*_SUITE_data` are built at **test run time**, not by `make` here: `ts_install_cth` (suite hook) calls `ts_lib:make_non_erlang/2`, which turns each `Makefile.src` into a `Makefile` by substituting `@CC@`, `@ERTS_CFLAGS@`, `@ERTS_LIBS@`, `@DEFS@`, `@erl_include@`, `@erts_lib_include_internal*@`, `@cross@`, `@DS@`, `@exe@`, `@IFEQ@` from `$TESTROOT/test_server/variables`.
- `ERL_COMPILE_FLAGS += +nowarn_missing_spec_documented` — suite callbacks need not be fully spec'd.
- New suites/helpers must be added to `MODULES`/`ERTS_MODULES` **and** be self-contained after release (anything not installed is unavailable at run time).

## Design Decisions
- System tests live here rather than in `../emulator/test` because they validate the *installed product* (launchers, install layout, release upgrade, cross-app consistency) and need the release tooling; `../emulator/test` compiles helpers against emulator internals directly.
- The C `ethread`/`erl_print` tests are here (not in `../emulator/test`) because they link against the installed static erts libraries and headers (`-lerts_internal_r`, `@erts_lib_include_internal*@`), i.e. they test what a 3PP/driver author can build against.
- `otp_versions.table` is copied into `otp_SUITE_data` (rather than read from the source tree) because `otp_SUITE`'s `test_runtime_dependencies_versions` needs the app-version → OTP-release map on a test host that has no source checkout; `otp_version_tickets` (containing `DEVELOPMENT` for non-tagged builds) lets it relax version checks for development builds.
- `system.spec` uses `../system_test` because ct_run resolves the suite path relative to the spec file, which itself is installed *into* the `system_test` directory — and `ts`-style invocations (`ts:run(system, ...)`) expect the suites under `$TESTROOT/system_test`.
- `upgrade_SUITE` guards on `release_handler:which_releases()`/`test_server:is_release_available/1` and skips instead of failing when the test host lacks a real (or older) release.

## Known Issues
- `ignore_cores.erl` is **not** in `MODULES`, is not compiled here, is not installed, and no suite in this directory references it (the compiled counterparts live in `../emulator/test/Makefile` and `../lib/kernel/test/Makefile`).
- `nt_SUITE` is effectively Windows-only: most cases skip unless `os:type() == {win32,nt}` (and some need admin privileges / pre-Win8), so it is near-noop on Unix CI.
- `ethread_SUITE` and `erl_print_SUITE` silently skip when the C compiler used for the test differs from the one that built OTP's static libs (`system.dynspec`); on Windows this is the common msc/gnu mismatch.
- `z_SUITE` must be the last suite executed and depends on `OTP_DAILY_BUILD_TOP_DIR`/`$ERL_TOP` to locate core files; with those unset it cannot find anything and reports success.
- `parallel_messages_SUITE` is a benchmark with a 90-minute timetrap — long-running and noise-sensitive on shared CI machines.
- `upgrade_SUITE` requires an older installed OTP release (or `{otp_releases, Rel}` in the CT config); otherwise cases skip with "no old release available".

## Notes for Agents
- Run from `erts/`, not `erts/test/`: `make/test_target_script.sh` maps `basename $PWD == erts` to `APPLICATION=system`, so the spec looked up is `$ERL_TOP/erts/make_test_dir/system_test/system.spec`. From `erts/test/` it would look for a non-existent `make_test_dir/test_test/test.spec`.
- The data-dir C programs are compiled in-place in the *installed* suite tree; a stale cached `$ERL_TOP/lib/common_test/test_server/variables.$TYPE.$FLAVOR` prevents `ts:install()` from re-running and therefore from recompiling data dirs with new flags — delete it when C flags change.
- Nothing **in this repository** consumes `*.dynspec` (grep for `dynspec` only finds the spec files themselves and the `Makefile` install rule); treat `system.dynspec` as a hint/legacy artifact and don't go hunting for a handler.
- Test-time environment knobs: `TS_RUN_EMU=asan|valgrind`, `VALGRIND_LOG_DIR`, `ASAN_LOG_DIR`, `CTRUN_TIMEOUT`, `EXTRA_ARGS`, `OTP_DAILY_BUILD_TOP_DIR` (see `HOWTO/TESTING.md`).

## Dependencies
- `../../lib/common_test` `test_server` (`ts`, `ts_run`, `ts_install`, `ts_install_cth`, `ts_lib`, `ts_make`), `ct_run` from the released tree, `sasl`/`release_handler` (upgrade), `xref`/`tools` (otp_SUITE), `stdlib` assert macros.
- A C compiler plus the OTP static erts libs/headers for the `Makefile.src` data dirs; `perl` for `run_erl_SUITE`; Windows toolchain for `nt_SUITE`.

## Test Strategy
- Whole ERTS system test (releases OTP first, then runs `system.spec`):
  - `cd erts && make test` — or from `$ERL_TOP`: `make system_test` (`system_test:` target does `cd erts && make test`).
- Single suite / case: `cd erts && make test ARGS="-suite otp_SUITE"` and `ARGS="-suite erlexec_SUITE -case args_file"` (ct_run runs with cwd = the installed `system_test` dir).
- Fast subset: `cd erts && SPEC_POSTFIX=smoke make test` (selects `system_smoke.spec`).
- From an installed test tree (HOWTO/TESTING.md flow): `ts:install().` then `ts:run(system, otp_SUITE, [batch]).`
- Release-only behavior: `make test` here compiles the data dirs and releases; a plain `make` in this directory only builds the `*.beam` and copies `install_bin`.
- CI: `.github/workflows/reusable-system-test.yaml` runs `make system_test` in a container with `SPEC_POSTFIX=gh`, `OTP_DAILY_BUILD_TOP_DIR=$PWD/make_test_dir`, coredumps enabled and `cth_surefire` JUnit output; `.github/workflows/reusable-test.yaml` runs the per-app tests (`emulator`, `epmd`, ...).

## Routing Table
- `otp_SUITE.erl` + `otp_SUITE_data/` → whole-release xref/version consistency; `otp_versions.table` & `otp_version_tickets` are copied in here by `release_tests_spec`
- `install_SUITE.erl` + `install_SUITE_data/` → install/symlink layout assertions (generated `install_bin`)
- `erlc_SUITE.erl` + `erlc_SUITE_data/` → `erlc` driver and feature-flag fixtures
- `erlexec_SUITE.erl` + `erlexec_SUITE_data/` → `erlexec` argv/env parsing (C argv dumper)
- `run_erl_SUITE.erl` + `run_erl_SUITE_data/` → `run_erl`/`to_erl` (Perl helpers)
- `upgrade_SUITE.erl` + `upgrade_SUITE_data/` → release upgrade / `release_handler`
- `ethread_SUITE.erl` + `ethread_SUITE_data/` → C `ethread` smoke tests against static erts libs
- `erl_print_SUITE.erl` + `erl_print_SUITE_data/` → C `erl_printf` formatting tests (`erts_test_utils` consumer)
- `nt_SUITE.erl` + `nt_SUITE_data/` → Windows NT service/event-log tests
- `z_SUITE.erl` → last-suite core-dump scan (no data dir)
- `parallel_messages_SUITE.erl` + `parallel_messages_SUITE_data/` → message/signal throughput benchmark and HTML visualizer
- `utils/` → `gccifier` compiler wrapper shared by all data-dir `Makefile.src` files (read-only support code)
- Specs: `system.spec` (all), `system_smoke.spec` (fast subset), `system.dynspec` (conditional skips)

## See Also
- `../emulator/test/` → VM-level suites and the source of `erts_test_utils.erl`
- `../epmd/test/` → `epmd_SUITE`
- `../../make/test_target_script.sh`, `../../make/install_bin` → the release/test driver used by `make test`
- `../../lib/common_test/test_server/` → `ts*` framework, `ts_install_cth` suite hook, generated `variables`
- `../../HOWTO/TESTING.md` → running released tests, ASan/Valgrind test runs
- `../../.github/workflows/reusable-system-test.yaml` → CI invocation of `make system_test`
