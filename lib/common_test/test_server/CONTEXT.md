# test_server — legacy Test Server driver (build/install/config layer for OTP's own tests)

## Intent
Holds the "old Test Server" frontend (`ts`, `ts_*`) that is still the build/install/run driver for running OTP's own test suites.
It does **not** implement the runtime test server any more — the runtime is `common_test` (`../src/test_server.erl`, `../src/test_server_ctrl.erl`, …).
Its `ts_*` modules are compiled in place (beams land in this directory) and are put on the code path only when this dir is passed to `-pa`/`-pz` (see `../../make/test_target_script.sh`) or installed under `$(TESTROOT)/test_server`.
It is *not* an OTP application and *not* an application subdirectory of `common_test` (the app's `SUB_DIRECTORIES` is `doc src priv proper_ext`); it is built through the test/release path only.

## API Surface (modules, all flat in this dir — no subdirectories)
- `ts.erl` — user-facing frontend / REPL entry: `install/0,1`, `run/0..5`, `run_category/1..3`, `tests/0,1`, `suites/1`, `categories/1`, `estone/0,1`, `cross_cover_analyse/1`, `compile_testcases/0,1`, `compile_datadirs/2`, `cl_run/1` (`erl -s ts cl_run App Cat <opts>`), `help/0`, plus backwards-compat `bench/*`, `benchmarks/0`, `smoke_test/*` and interactive shell helpers `i/0`, `l/1`, `r/0..3`.
  - `run/*` consults the `variables` file, then goes through `ts_run`; `run_category(...,bench)` goes through `ts_benchmark`.
- `ts_install.erl` — `install/2` (`install_local` or a target os type) and `platform_id/1`: runs `./configure` (Unix) or `ts_autoconf_win32:configure/0` (Win32), parses `conf_vars`, enriches it via `ts_erl_config:variables/2` and writes the term file `variables`.
  - Cross-compilation: `target_install/1` detects `{cross,"yes"}` in `variables` and renames it to `variables-cross`.
- `ts_run.erl` — `run/4` and `ct_run_test/2`: builds the Erlang command line that runs the tests (`-name`/`-sname`, `-rsh`, PATH/`ERL_CRASH_DUMP`, `-config ts*.config`, `-ct_test_vars {net_dir,_}`, `-pz <cwd>`), runs `Makefile.first` pre-makefiles, then executes either batch (`open_port`) or interactive (new window); the spawned node calls `ts_run:ct_run_test/2`, a thin wrapper around `ct:run_test/1`.
- `ts_lib.erl` — shared utilities: `var/2`, `verbosity`, `progress/4`, `erlang_type/0,1` (otp vs srctree), test discovery (`specs/1`, `suites/2`, `test_categories/2`, `specialized_specs/2`), `subst*` (`@var@` substitution incl. `@IF[@N]EQ@` conditionals), and `make_non_erlang/2` (runs `all_SUITE_data/Makefile.first`, substitutes `Makefile.src` → `Makefile`, then runs `make`).
- `ts_make.erl` — `make/1,3` and `unmake/1`: wrappers for invoking `make` from test cases / config code.
- `ts_erl_config.erl` — `variables/2`: probes the Erlang installation/source tree (erl_interface, `ic`, jinterface, ssl, erts include+lib paths, thread type, debug marker, shlib rules) and adds those key/value pairs to `variables`.
- `ts_autoconf_win32.erl` — `configure/0`: a hand-written Windows replacement for autoconf that produces the same `conf_vars` keys.
- `ts_install_cth.erl` — a Common Test **hook** (`id/1`, `init/2`, all `pre/post_*_per_*` callbacks, `on_tc_fail/3`, `on_tc_skip/3`, `terminate/1`).
  - This is the one module here used directly by suites: `{ct_hooks,[ts_install_cth]}` appears in the `suite/0` of hundreds of OTP suites.
  - It locates `variables` in the sibling `test_server` dir, calls `ts_lib:make_non_erlang/2` to build suite data dirs (`Makefile.src`), skips data-dir building for cross-compiled tests, injects a node name into `Config`, and kills slave nodes on terminate.
- `ts_benchmark.erl` — `benchmarks/0`, `run/3` plus a `gen_event` handler: runs bench specs via `ts_run:run/4` with itself as event handler, collects `benchmark_data` events into `<timestamp>/*.ebench` and writes `latest_benchmark`.

## Data / config files
- `ts.hrl` — log-directory and log/summary file-name macros; the header says these must stay identical to the ones in `test_server` (`../src/`).
- `ts.config`, `ts.unix.config`, `ts.win32.config` — Common Test config files passed to `ct_run` via `-config` (test host definitions, etc.).
- `cross.cover` — cover specification used by `ts:cross_cover_analyse/1` (which modules to cover but exclude from cross analysis).
- `conf_vars.in` (+ `.license`) — template for the autoconf-generated `conf_vars` file (compiler, shlib, EI/erts flags, target host, `make_command`, …); its keys are exactly what `ts_install:autoconf2/1` parses.
- `configure`, `configure.ac`, `config.guess`, `config.sub`, `install-sh` — this dir's own autoconf setup (autoconf 2.72), separate from `../configure` (which only generates `../priv/$host/Makefile`).
- `vendor.info` — SPDX vendor metadata pointing at `config.guess`/`config.sub`/`install-sh` (Autoconf, FSF).
- `.gitignore` — ignores generated `conf_vars`, `config.log`, `config.status`, `variables*`.

## API Surface (build & release)
- `Makefile` — compiles `TS_MODULES` (`ts ts_run ts_lib ts_make ts_erl_config ts_autoconf_win32 ts_install ts_install_cth ts_benchmark`) into `EBIN=.` (beams next to the sources); `release_tests_spec` installs the `.erl`/`.hrl`/`.beam`/`conf_vars.in`/`ts*.config` files plus `configure config.sub config.guess install-sh` into `$(RELEASE_PATH)/test_server`.
- Entered from: `Makefile.in` `TEST_DIRS` (first entry `lib/common_test/test_server`) for `make tests` / `make release_tests` / `otp_build tests`; `otp_build` (`autoconf_aux_dirs`, `AUTOCONF_SUBDIRS` incl. `lib/common_test/test_server`); `scripts/otp_build_check` (test target `test_server`); `make/test_target_script.sh` (root `make test` and per-app `make test`).
- Docs that rely on this dir: `../../HOWTO/TESTING.md` (`ts:install()`, `ts:run/...`), `../../HOWTO/INSTALL-CROSS.md` (`ts:install([{xcomp,...}])` + `ts compile_testcases`), `../../.github/dockerfiles/Dockerfile.cross-compile`.

## Constraints
- Compiles with `-Werror`; keep it warning-free, hence the deliberate `-compile([{nowarn_possibly_unsafe_function,…},{nowarn_unsafe_function,…}])` and `nowarn_deprecated_catch` pragmas — do not "clean them up" carelessly.
- Legacy code: do not modernize/restructure casually; it must keep compiling and behaving as-is because `make test`, cross-compile CI and `ts_install_cth` depend on it.
- `configure`/`config.guess`/`config.sub`/`install-sh` are generated or vendored — regenerate via `./otp_build update_configure` (autoconf 2.72) instead of editing by hand.
- New behaviour belongs in `common_test` (`../src/`) or in a proper CT hook; do not add modern test-server logic here.

## Dependencies
- GNU autoconf 2.72 (matching `otp_build`'s `USE_AUTOCONF_VERSION`) plus `config.guess`/`config.sub`/`install-sh` from the repo (`../../make/autoconf/`).
- A working C compiler / `make` on the host, since `ts:install/1` runs `./configure` and `ts_lib:make_non_erlang/2` runs `make` on suite data dirs.
- The `common_test` application itself (the spawned nodes use `ct:run_test/1`, `ct:get_progname/0`, `ct_event.hrl`).

## Notes for Agents
- **Not the runtime Test Server.** The live implementation is `../src/test_server.erl`, `../src/test_server_ctrl.erl`, `test_server_io/gl/node/sup.erl` inside `common_test/ebin`. `test_server/` only drives, configures and launches it.
- These modules exist only in this directory — they are **not** in `common_test`'s `ebin`, so `ts_install_cth` is resolvable only when this dir is on the code path (as `make test` does with `-pa`/`-pz $ERL_TOP/lib/common_test/test_server`) or after `release_tests`.
- `ts_install_cth:pre_init_per_suite/3` defaults `ts_conf_dir` to `<data_dir>/../../test_server` and consults `variables` there; the `variables` file is produced by `ts:install()` and is required for building suite data dirs.
- `ts.hrl` macros must stay in sync with the `test_server` module in `../src/`.
- The diagram comment at the top of `ts.erl` still names modules that no longer exist (`ts_filelib`, `ts_make_erl`, `ts_autoconf_cross_env`) — treat it as stale.
- `EBIN=.`: the compiled `.beam` files live beside the sources here, so this directory is not a pure source dir.

## Test Strategy
- No test suites live here; the directory is exercised indirectly by OTP's own test runs.
- Validation is `make && erl -noshell -eval "ts:install()." -s init stop` in this dir (as `make/test_target_script.sh` does), then running suites (e.g. `ts:run(App,[batch])` or `ct_run` with `-pa` of this dir).
- Failures surface as broken suite data-dir compilation (`Makefile.src`) or as `variables`-consult errors in `ts_install_cth`.

## See Also
- `../src/` — the real `common_test` implementation (`test_server.erl`, `test_server_ctrl.erl`, `ct*.erl`); `ts_run:ct_run_test/2` ultimately calls `ct:run_test/1` there (read-only sibling; escalate writes to `./lib/common_test/`).
- `../test/` — `common_test` test suites, most of which use `{ct_hooks,[ts_install_cth]}` and thus depend on this dir being on the code path (read-only sibling).
- `../include/` — `ct.hrl`/`ct_event.hrl` consumed here via `-I../include` and `include_lib` (read-only sibling).
- `../../make/test_target_script.sh` — the `make test` driver that compiles and invokes `ts` from this dir.
- `../../Makefile.in` (`TEST_DIRS`), `../../otp_build`, `../../scripts/otp_build_check` — release/build entry points that build and install this dir.
- `../../HOWTO/TESTING.md`, `../../HOWTO/INSTALL-CROSS.md` — user-facing documentation of the `ts:*` workflow.
