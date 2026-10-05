# sasl — Test Suite

## Intent
Common Test suites and fixture data for the `sasl` application (`systools`, `release_handler`, `alarm_handler`, `rb`, `sasl_report`, `sasl` app callbacks).
Suites exercise both pure library functions (`systools` script/tar/relup generation) and destructive, reboot-level release upgrade flows (`release_handler`) that mutate an installed release tree.

## API Surface
`Makefile` drives the tests:
- `MODULES` = `sasl_SUITE alarm_handler_SUITE installer release_handler_SUITE sasl_report_SUITE sasl_report_suite_supervisor systools_SUITE systools_rc_SUITE rb_SUITE rh_test_lib otp_vsns` — everything compiled into the release's `sasl_test` dir; `installer`, `rh_test_lib`, `otp_vsns`, `sasl_report_suite_supervisor` are **helper libs, not suites**.
- `EXTRA_FILES = $(ERL_TOP)/otp_versions.table` — copied alongside the tests and read by `otp_vsns`.
- `RELSYSDIR = $(RELEASE_PATH)/sasl_test` — tests are installed as the `sasl_test` release dir and run from there.
- `make_emakefile` generates `Emakefile` via `$(ERL_TOP)/make/make_emakefile` (all `*_SUITE_make` + `$(MODULES)`); `+deterministic` is filtered out of `ERL_COMPILE_FLAGS` for the test build.
- `release_tests_spec` installs sources+`sasl.spec`+`sasl.cover`+`Emakefile`, tars **every** `*_SUITE_data/` dir into `$(RELSYSDIR)`, and additionally copies `$(ERL_TOP)/make/otp_version_tickets` into `sasl_SUITE_data/`.
- `sasl.spec` = `{suites,"../sasl_test",all}` (run all suites from the installed `sasl_test` dir).
- `sasl.cover` = `{incl_app,sasl,details}` (coverage config).

Helper modules (compiled, not run as suites):
- `otp_vsns.erl` — parses the repo-root `otp_versions.table`; `read_state/0` looks for it next to the module (`code:which/1` dirname, else `../priv/`), `read_state/1` by path; `branch_latest/2`, `branch_vsns/2`, `app_vsn/3` answer "which app versions shipped in which OTP release" (used by `sasl_SUITE` and `release_handler_SUITE` for version-format history tests).
- `installer.erl` — a large custom install/upgrade driver (`install_1/2`..`install_14/1`, `upgrade_restart_*`, `client1_1/4`, `client2/3`, `stop/1`, …) invoked by `release_handler_SUITE` as external node scripts to install/upgrade/restart releases across reboots.
- `rh_test_lib.erl` — shared `release_handler` test helpers: `cmd/3` (spawn a command), `erlsrv/3,4` + `get_service_args/*` + `get_start_erl_args/*` (Windows service / erlsrv handling), `get_client_args/3,4`, `clean_dir/1,2`, `old_app_vsn/1`.
- `sasl_report_suite_supervisor.erl` — one-for-one supervisor (`start_link/0`) that just hosts `sasl_report_SUITE:start_link/0` as a child for the crash-reporting tests.

## Suites
- `sasl_SUITE` — `log_mf_h_env, log_file, app_test, appup_test, utc_log`; sets `kernel:logger_sasl_compatible` in `init_per_suite`; uses `otp_vsns` + `sasl_SUITE_data/otp_version_tickets`.
- `alarm_handler_SUITE` — `set_alarm, clear_alarm, swap`; adds itself as a `gen_event` handler to observe alarms.
- `rb_SUITE` — `help, start_error_stop` plus `{group,running_error_logger}` (`suppress`/`shuffle`) with `show, show_other, list, rescan, start_stop_log, grep, filter_filter, filter_date, filter_filter_and_date, filter_re_no`; installs `log_mf_h` with a temp `error_logger_mf_dir`.
- `sasl_report_SUITE` — crash-report formatting cases (`gen_server_crash*`, `legacy_gen_server_crash*`, latin1 vs unicode, char limits).
- `systools_SUITE` — the largest library suite; groups `script`, `tar`, `relup`, `hybrid`, `options` covering script/tar/relup generation, unicode, wildcards, optional/included apps, duplicate modules, regexp appups, links, erts tar.
- `systools_rc_SUITE` — `systools:make_script` etc. via the internal `#application{}` record API (`-include_lib("sasl/src/systools.hrl")`): `syntax_check, translate, translate_app, translate_emulator_restarts, translate_add_delete_module`.
- `release_handler_SUITE` — 119 KB, the destructive/reboot suite (see below).

## release_handler_SUITE structure & hazards
- `all()` branches on `os:type()`: `{unix,_}` → `unix_cases()`, `{win32,_}` → `win32_cases()`.
- `unix_cases()` runs `target_system, target_system_unicode`, then `{group,release}` **only if `$ROOT/bin/run_erl` exists**, otherwise the single `no_run_erl` (returns `{comment, "No run_erl program"}`); then the all-platform `cases()`.
- `cases()` (all platforms): `otp_9395_check_old_code, instructions, eval_appup, eval_appup_with_restart, install_release_syntax_check, otp_10463_upgrade_script_regexp, no_dot_erlang, move_system, {group,absolute}, {group,relative}`.
- Groups: `absolute`/`relative` run the same `root_dir_cases()` with/without an absolute root dir; `release` → `release_single` (`upgrade, upgrade_restart, client1, client2`) and `release_gg` (`upgrade_gg`, global-group related).
- `{group,release}` `init_per_group` **skips on Win8+** ("Requires admin privileges"), and setups build the `p1g`/`p1h`/`p1i`/`p2a` release fixtures under the test `priv_dir`.
- These cases actually install/reboot releases (via `installer` + `run_erl`), so they are **slow and mutate the installed release tree**; needs `$ROOT` to be a real installed release.

## Fixtures
Suites find data via `?config(data_dir, ...)`; the tar step in `Makefile` copies each `*_SUITE_data/` under `$(RELSYSDIR)`.
- `release_handler_SUITE_data/` — inputs and scaffolding for the reboot-level tests:
  - `lib/` — fake apps with pre-baked `ebin`: `a-1.0/1.1/1.2/9.0/9.1`, `b-1.0/2.0`, `many_mods-1.0/1.1/2.0`, `installer-1.0`.
  - `app1_app2/` — `lib1..lib4` layout permutations of `app1`/`app2` for lib-dir/search-path tests.
  - `unicode/u-1.0`,`u-1.1` — unicode-named app fixtures (`unicode_upgrade`).
  - `relocatable_release/` — a hello_server release built from `.src` templates (relocatable root-dir test).
  - `release_handler_timeouts/` — a `dummy-0.1` app with sup/server modules for timeout tests.
  - `regexp_appup/` — `app1` with a version-regexp `.appup` (`otp_10463_upgrade_script_regexp`).
  - `otp_2740/` — `vsn_atom/list/numeric/string/tuple.erl` version-format fixtures.
  - `c/` (`aa.erl`, `b.erl`, `c.app`, `c_sup.erl`) — small module loaded/unloaded in old-code checks.
  - `Makefile.src` (compiles the fake `lib/` apps, uses `@EMULATOR@`), `erl.ini.src`, `start`/`start_client` (`run_erl`/`start_erl` embedded-start scripts, `%ROOT%`/`%CLIENTARGS%` templated), `heart_restart.bat` (Windows), `target_system.erl` → symlink to `../../examples/src/target_system.erl`.
- `systools_SUITE_data/` — input trees fed to `systools` **without a full build** (pre-baked `ebin/*.app` + `.appup` + `src/`):
  - `d_normal` — valid multi-app/multi-vsn tree (`db-*`, `fe-*`); `d_links` — symlinked modules (`link_tar`).
  - `d_missing_src`, `d_no_appup`, `d_bad_appup`, `d_bad_app_vsn`, `d_bad_mod+warn`, `d_replace_app`, `d_opt_apps` — error/edge inputs for script/tar/relup generation.
  - `d_duplicate_modules`, `d_regexp_appup`, `d_unicode` — duplicate-module, regexp-appup and unicode-named-app cases.
  - `lib/` — minimal fake release tree `kernel`, `stdlib`, `sasl` (+ `sasl-9.9`) providing `.app`/`.appup` for script/relup tests.

## Test Strategy
- Run from the `sasl` app dir: `make test` (app Makefile sets `TEST_NEEDS_RELEASE=true`, `DIA_PLT_APPS=tools`), i.e. tests need a **fully built + installed OTP release** (`$ROOT`), not just compiled beams.
- Single suite: `ct_run -suite sasl_test/systools_SUITE` from inside `$(RELEASE_PATH)` (or `ct` with `-dir`); suites live in the installed `sasl_test` dir, not in-tree.
- `systools*`/`sasl`/`rb`/`alarm_handler`/`sasl_report` are fast library tests; `release_handler_SUITE` is the slow, stateful one (reboots, run_erl, root-dir manipulation).
- `sasl_SUITE:appup_test` depends on `sasl_SUITE_data/otp_version_tickets` + `otp_versions.table`, so a partial checkout without those files cannot run it.

## Known Issues
- `release_handler_SUITE` `{group,release}` is **skipped on Win8+** (needs admin) and on unix when `run_erl` is absent (`no_run_erl`); unix `release` cases also require `$ROOT` to be a real installed release.
- The reboot/install cases mutate the installed release tree — running them consecutively in one workspace can leave stale state; they rely on `priv_dir` scaffolding that is rebuilt per group.
- `unicode` cases need an OS/filesystem that supports unicode filenames; `sasl_report` unicode cases exercise char/byte limits in report formatting.

## Routing Table
| Path | Purpose |
| --- | --- |
| `sasl_SUITE.erl` | Core sasl app callbacks, log handling, appup-vs-vsn history (`otp_vsns`) |
| `alarm_handler_SUITE.erl` | `set_alarm`/`clear_alarm`/`swap` event handling |
| `rb_SUITE.erl` | Report browser (`rb`) incl. `running_error_logger` group |
| `sasl_report_SUITE.erl` | Crash/supervisor report formatting (latin1/unicode) |
| `systools_SUITE.erl` | `systools` script/tar/relup/hybrid/options (library-level) |
| `systools_rc_SUITE.erl` | `systools` via internal `#application{}` record API |
| `release_handler_SUITE.erl` | Destructive install/upgrade/reboot cases (run_erl root-dir groups) |
| `installer.erl` | Helper lib: external install/upgrade driver used by release_handler tests |
| `rh_test_lib.erl` | Helper lib: shared release_handler helpers (cmd, erlsrv, client args, clean_dir) |
| `otp_vsns.erl` | Helper lib: parses `otp_versions.table` for OTP-vsn↔app-vsn history |
| `sasl_report_suite_supervisor.erl` | Helper lib: supervisor hosting `sasl_report_SUITE:start_link/0` |
| `sasl.spec` / `sasl.cover` | Test spec (`{suites,"../sasl_test",all}`) and coverage config |
| `release_handler_SUITE_data/` | Fake release trees + scaffolding for reboot-level tests (`lib/`, `app1_app2/`, `unicode/`, `relocatable_release/`, `otp_2740/`, start scripts) |
| `systools_SUITE_data/` | Pre-baked `d_*` app trees + minimal `lib/` used as systools inputs |

## See Also
- `../` (`lib/sasl/CONTEXT.md`) — the application under test (`systools`, `release_handler`, `alarm_handler`, `rb`).
- `../../examples/src/target_system.erl` — target of the `release_handler_SUITE_data/target_system.erl` symlink.
- `../../make/otp_version_tickets`, repo-root `otp_versions.table` — version-history inputs copied into the release.
