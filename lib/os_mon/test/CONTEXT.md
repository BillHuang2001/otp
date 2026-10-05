# os_mon — Common Test Suites

## Intent
Common Test suites exercising the OTP `os_mon` application's monitors (`cpu_sup`, `disksup`, `memsup`, `os_sup`) and the `os_mon` application resource/config.
Suites are heavily OS-conditional: each selects a different case list per `os:type()`, so a case that "fails" or is skipped may simply be unsupported on the host OS.

## API Surface
- `cpu_sup_SUITE.erl` — CPU-load/utilization monitor: `load_api`, `util_api`, `util_values`, `port`, `unavailable`.
- `disksup_SUITE.erl` — disk-space monitor: `api`, `config`, `alarm`, `port`, `terminate`, `unavailable`, `restart`, `otp_5910`, `posix_only`, `parse_df_output_posix`, `parse_df_output_susv3`.
- `memsup_SUITE.erl` — memory monitor: `api`, `alarm1`, `alarm2`, `process`, `config`, `timeout`, `unavailable`, `port`, `otp_5910`, `improved_system_memory_data`.
- `os_sup_SUITE.erl` — OS-message (syslog/eventlog→Erlang `error_logger`) monitor: `message`, `config`, `port`; helpers `test_mfa/2` and `message_receptor/0`.
- `os_mon_SUITE.erl` — application-level: `app_file`/`appup_file` (via `test_server:app_test|appup_test`) and `config` (the `start_cpu_sup`/`start_disksup`/`start_memsup` env flags gate which child processes are registered).
- `os_mon_conf.erl` — tiny `init/1`/`fin/1` module; starts `os_mon` with `temporary` and records the result, `fin/1` stops it. Used as a test-suite config hook (Comet/`ct` conf style), not a CT suite.
- `os_mon.spec` — full run: `{suites,"../os_mon_test",all}` (path is relative to the installed `$RELEASE_PATH/os_mon_test` dir).
- `os_mon_smoke.spec` — quick subset: `disksup_SUITE:[api]`, `cpu_sup_SUITE:[load_api,util_api]`, `memsup_SUITE:[api]`.
- `os_mon.cover` — coverage spec `{incl_app,os_mon,details}` for `cover`/`ct` runs.

## OS Gating (which cases run where)
- `cpu_sup_SUITE`: full set on `{unix,sunos|linux|freebsd|openbsd|darwin}`; only `unavailable` on `{unix,netbsd}`; only `load_api` on other `{unix,_}`; only `unavailable` everywhere else.
- `disksup_SUITE`: the full `Always` list on any `{unix,_}` and `{win32,_}`; only `unavailable` on anything else.
- `memsup_SUITE`: `{unix,sunos}` gets `[api,alarm1,alarm2,process,config,timeout,unavailable,port]`; `{unix,linux}` gets `[api,alarm1,alarm2,process,timeout]`; all other OSes only `[api,alarm1,alarm2,process]`; `otp_5910` and `improved_system_memory_data` always run.
- `os_sup_SUITE`: `[message,config,port]` only on `{unix,sunos}`; `[message]` on `{win32,_}`; whole suite `{skip, ...}` elsewhere (no skips inside `all/0` on win32, so `config`/`port` never run there).
- `os_mon_SUITE`: `config` only on `{unix,sunos}`; `app_file`/`appup_file` everywhere.
- `improved_system_memory_data` asserts `available_memory` presence on Linux (only if `/proc/meminfo` has `MemAvailable`) and Darwin; returns `ok` on all other OSes.

## Runtime Skips (not platform-gated but data/privilege dependent)
- `cpu_sup_SUITE:util_values` → `{skip, "CPU utilization was too high (~f%)"}` when the host is already busy (baseline utilization above `(100/NrOfProcessors)*0.33`).
- `disksup_SUITE:port` / `memsup_SUITE:port` / `cpu_sup_SUITE:port` / `os_sup_SUITE:port` → `{skip, {os_pid_not_found,_}}` or `{skip, {not_killed, Line}}` when the helper process cannot be found/killed via `ps`+`kill -9`; they `ct:fail` only after an unexpected outcome.
- `memsup_SUITE:alarm1`/`alarm2` → `{skip, sys_mem_too_high}` when system memory usage is already `> 0.99`.
- `disksup_SUITE:alarm`/`otp_5910` are inherently flaky by design: they fail if disk usage changes mid-run or if `alarm_handler` receives alarms late (see NOTE comments in the suite).
- `os_sup_SUITE:config` is effectively a no-op/probe: `os_sup_enable` and `os_sup_own`/`os_sup_syslogconf` cannot be tested because the test process does not run as root (Solaris syslog needs root).
- `os_sup_SUITE:port` locates the helper via `ps -e | grep '[f]errule'` — Solaris-only helper naming.

## Constraints / Conventions
- Tests mutate global application env of the running `os_mon` app (`application:set_env(os_mon,...)`) and restart individual children via `supervisor:terminate_child/restart_child(os_mon_sup, ...)`; cases reset env to defaults before returning.
- Most suites `init_per_suite` starts `os_mon` and `end_per_suite` stops it; `os_mon_conf.erl` is the alternative hook that does the same when a suite config requires it.
- Suites use `ts_install_cth` (a CT hook supplied by the installed release) and `{timetrap,{minutes,1}}`.
- `parse_df_output_posix`/`parse_df_output_susv3` are pure unit tests of `disksup:parse_df/2` (no OS dependency), so they run on every platform.
- `otp_5910` appear in both `disksup_SUITE` and `memsup_SUITE` (regression: alarms cleared, not doubled, on monitor crash / `os_mon` stop).

## Build & Run
- `test/Makefile` expands `MODULES` into a generated `Emakefile` via `$(ERL_TOP)/make/make_emakefile`, compiles with `ERL_COMPILE_FLAGS += -I$(ERL_TOP)/lib/snmp/include +nowarn_missing_spec_documented` (`+deterministic` is filtered out).
- `make tests` (alias `make test`) builds the suites into `ebin == .` in this dir.
- `make release_tests` installs `os_mon.spec`, `os_mon.cover`, `os_mon_smoke.spec`, the generated `Emakefile` and all suite sources into `RELSYSDIR = $(RELEASE_PATH)/os_mon_test`.
- No `*_SUITE_data` directories exist (the tar rule in the Makefile is commented out), so suites need no data files.

## Notes for Agents
- Because cases are selected by `os:type()` inside each suite's `all/0`, run/skip differences across platforms are expected; do not treat a platform-specific case list as a bug.
- The `port` cases need a working `ps`/`grep`/`kill -9` and the ability to read the helper process line; they are naturally skipped in restricted/container environments.
- `os_sup_SUITE` sets `os_sup_mfa` to `{?MODULE,test_mfa,[?TAG]}` to capture messages, and feeds a fake syslog line directly to `os_sup_server ! {faked_port,{data, Data}}` rather than depending on a real syslog.
- Windows-only message parsing is covered inside `os_sup_SUITE:message` (the `{win32,_}` branch), which is the only case that runs on win32.

## Test Strategy
- `make test` in this dir runs the Common Test suites in place; `os_mon.spec` is the CI/installed-release driver (`all` suites), `os_mon_smoke.spec` a fast subset.
- Coverage is collected via `os_mon.cover`.
- Cases needing an installed release/helper port programs (`port`, and `ts_install_cth`-dependent suites) are validated in release-test runs rather than ad-hoc single-suite runs.

## See Also
- `../` — the `os_mon` application sources (`src/cpu_sup.erl`, `disksup.erl`, `memsup.erl`, `os_sup*`, `os_mon_sup`).
- `../CONTEXT.md` — application-level intent and API surface for `os_mon`.
- `../../make/make_emakefile`, `../../make/otp_release_targets.mk` — build/target infrastructure used by `test/Makefile`.
