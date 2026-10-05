# SASL — System Architecture Support Libraries

## Intent
OTP application providing the system-architecture support services: release handling (`systools` build-time generation of `.script`/`.boot`/`tar`/`relup`; `release_handler` runtime unpack/install/upgrade/remove of releases), alarm handling (`alarm_handler`), the Report Browser (`rb`) and legacy SASL error-logging support.
"SASL" here has **nothing** to do with "Simple Authentication and Security Layer" (RFC 4422).
Pure Erlang: no `configure`, no `c_src/`, no NIFs.
Version `4.4` (`SASL_VSN` in `vsn.mk`); OTP release 30.0-rc0.

## API Surface
The publicly documented API is exactly the four modules that carry a `-moduledoc`:
- `systools` — `make_script/1,2`, `make_tar/1,2`, `make_relup/3,4`, `script2boot/1,3`, `compile_rel/3`.
- `release_handler` — `create_RELEASES/1..4`, `unpack_release/1`, `check_install_release/1,2`, `install_release/1,2`, `new_emulator_upgrade/2`, `remove_release/1`, `which_releases/0,1`, `make_permanent/1`, `reboot_old_release/1`, plus the test-only `upgrade_app/2`/`downgrade_app/2,3`/`eval_appup_script/4`.
- `alarm_handler` — `set_alarm/1`, `clear_alarm/1`, `get_alarms/0`.
- `rb` — `list/0,1`, `show/0,1`, `grep/1`, `filter/1,2`, `start_log/1`, `stop_log/0`.
App-level surface: `vsn.mk` (`SASL_VSN`), `src/sasl.app.src` (registered `sasl_sup`/`alarm_handler`/`release_handler`; `applications [kernel, stdlib]`; `mod {sasl,[]}`), `src/sasl.appup.src` (regex-versioned up/downgrade rules using `restart_new_emulator`).
Application configuration (all optional, see `doc/sasl_app.md`): `start_prg`, `masters`, `client_directory`, `static_emulator`, `releases_dir` (falls back to the `RELDIR` OS env var, default `$OTPROOT/releases`).
File-format specs that define this app's contracts live in `doc/references/` (`rel.md`, `script.md`, `appup.md`, `relup.md`).

## Dependencies
`{applications, [kernel, stdlib]}`.
`{runtime_dependencies, ["tools-2.6.14","stdlib-4.0","kernel-6.0","erts-15.0"]}` — old minimum baselines; `tools` is needed because `systools` runs `xref` for the `exref` option (hence the app Makefile's `DIA_PLT_APPS=tools`).
External consumers: `erts/start_scripts/Makefile` (uses `systools` + `release_handler:create_RELEASES/4` to generate `RELEASES.src`, `start_sasl.boot`), `otp_patch_apply`, `lib/reltool`, `lib/os_mon` (`misc_supp:format_pdict/3`), and the `system/doc/design_principles/release_handling.md` / `create_target.md` docs.

## Build
`Makefile`: includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `vsn.mk` (`VSN = $(SASL_VSN)`), then `make/app_targets.mk`.
`SUB_DIRECTORIES = src doc examples/src` — `include/`, `priv/` and `ebin/` are **not** sub-built (no Makefile in them).
`TEST_NEEDS_RELEASE=true` — the test suites require a fully built+installed OTP release, not just compiled beams.
Build order: `sasl` is one of `ERTS_APPLICATIONS = stdlib sasl kernel compiler` in `lib/Makefile`, i.e. it is built immediately after `stdlib` and before `kernel`/`compiler`, because the erts start scripts and the rest of the build depend on `systools`/`release_handler`.

## Constraints
- `src/Makefile` holds an explicit `MODULES` list — a new module file is silently ignored unless added there.
- `ebin/sasl.app` and `ebin/sasl.appup` are generated from the `.src` templates by sed-substituting `%VSN%`; never edit the generated files.
- `systools.hrl` is an *internal* header (records `#release{}`/`#application{}`), shipped into the release under `src/` as `INTERNAL_HRL_FILES`; `include/` is intentionally empty (no public Erlang headers).
- Source compile flags: `-I../../stdlib/include -Werror +nowarn_deprecated_catch` — any compiler warning breaks the build.
- Runtime manipulation of paths under `$ROOT/releases` must go through `release_handler`'s `root_dir_relative_*`/`do_*` wrappers (client/master-node semantics), not raw `file:*`.
- `release_handler` must not be upgraded by release handling itself (fixed child spec with `modules = []` in `sasl.erl`).

## Design Decisions
Release handling is a two-layer model: high-level `.appup` instructions (`{update,Mod,Change,...}`, `{load_module,...}`, `{restart_application,...}`) are translated into low-level `script` instructions at package build time (`systools_relup` + `systools_rc`) or at runtime for direct `upgrade_app/2`; the low-level script is what `release_handler_1` evaluates.
A release is built as `.rel` → `.script`/`.boot` (+ `.tar.gz` package containing `.rel`, `.boot`, `relup`, app dirs, erts); installing it at runtime unpacks into `$ROOT/releases/<vsn>`/`lib/` and the persistent state (installed versions, permanent version, status unpacked/permanent/old/current) is recorded in the `RELEASES` file in the `releases_dir`.
The `release_handler` can run in client/master mode (`masters` + `client_directory` config) where one node coordinates release installation on several master nodes and a client directory.
`alarm_handler` is a `gen_event` manager so users can swap in their own handler (e.g. an SNMP/MIB one) via `gen_event:swap_handler/3`.
The deprecated `error_logger` machinery (`sasl_report*` handlers, `error_logger` config knobs) is retained only for `kernel`'s `logger_sasl_compatible` mode; the modern path is the `logger_std_h` handler named `sasl` (domain `[otp,sasl]`) installed by `sasl.erl`.

## Known Issues
- `rb` only shows reports written by stdlib's `log_mf_h` error-logger handler; with the default `logger` setup and no `error_logger_mf_dir`/`maxbytes`/`maxfiles` configured, there is nothing to browse.
- `erlsrv` is Windows-only and shells out to `erts-<Vsn>/bin/erlsrv.exe`.
- The `sasl_report_tty_h`/`sasl_report_file_h` handlers and the SASL error-logging concept are deprecated since OTP 21 (see `doc/guides/error_logging.md`).
- `priv/mibs/` and `priv/test/` are committed empty placeholders (only `.gitignore`); nothing in this app reads or writes them, and they are not installed.

## Notes for Agents
- Start release-handling work at `systools` (build time) or `release_handler` (runtime); everything else in that area is internal plumbing.
- Do not confuse this app with Erlang's SASL-based `kernel` authentication; the two are unrelated.
- The current app version is `SASL_VSN` in `vsn.mk`; the OTP-version ↔ app-version mapping lives in the repo-root `otp_versions.table` (read by `test/otp_vsns.erl`).
- Doc generation is ExDoc-driven from `doc/docs.exs` plus the globs in `make/ex_doc.exs`; only `-moduledoc`'d modules are published.

## Test Strategy
`make test` in this directory runs the Common Test suites, but `TEST_NEEDS_RELEASE=true` means a full `$ERL_TOP` build and an installed release are prerequisites.
Library-level suites (`systools_SUITE`, `systools_rc_SUITE`, `alarm_handler_SUITE`, `rb_SUITE`, `sasl_report_SUITE`, `sasl_SUITE`) are fast; `release_handler_SUITE` is slow and destructive (it reboots nodes via `run_erl`/`installer` and mutates the installed release tree).
See `test/CONTEXT.md` for the suite list, helper libs, fixture trees and platform skips.

## Routing Table
- `src/` → the 17 application modules (release handling, alarm handler, RB, legacy report support, Windows service helper) + `sasl.app.src`/`sasl.appup.src`/`systools.hrl`/`Makefile`
- `test/` → Common Test suites, helper libs (`installer`, `rh_test_lib`, `otp_vsns`, `sasl_report_suite_supervisor`), and the `*_SUITE_data/` release fixtures
- `doc/` → ExDoc sources: `guides/`, `references/` (normative `.rel`/`.script`/`.appup`/`relup` file-format specs), `sasl_app.md`, `notes.md`, `src/rel/` doc examples
- `examples/` → `src/target_system.erl` target-system example (installed under `doc/examples` by `release_docs`)
- `include/` → intentionally empty (no public headers)
- `priv/` → empty placeholders, not built or installed
- `ebin/` → build output (only `.gitignore` tracked)
- `vsn.mk` → defines `SASL_VSN` (app version)
- `Makefile` → app build/release driver

## See Also
- `../kernel/` — `logger`, `log_mf_h`, `application`, `release_handler`'s consumer of the SASL logger config
- `../stdlib/` — includes `erl_compile.hrl` used by `systools`; provides `log_mf_h` read by `rb`
- `../tools/` — `xref`, used by `systools` for `exref`
- `../reltool/` — release-management tool that builds releases semi-independently of `systools`
- `../../erts/start_scripts/` — generates the OTP start scripts and `RELEASES.src` with `systools`/`release_handler`
- `../../system/doc/design_principles/release_handling.md` — user-level release-handling documentation; `appup_cookbook.md` accompanies `doc/src/rel/`
- `../../otp_patch_apply`, repo-root `otp_versions.table` — patch application and version-mapping consumers
