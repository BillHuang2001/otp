# runtime_tools — Test Suites

## Intent
Common Test suites for the `runtime_tools` application: `dbg`, `instrument`, `msacc`, `scheduler`, `system_information`, `versions`, `observer_backend`, `dyntrace`, plus app-level smoke tests.
Suites are compiled in place in this directory and installed to `$(RELEASE_PATH)/runtime_tools_test` as a release test directory (`RELSYSDIR`).

## API Surface
- `Makefile` — `MODULES` lists the 11 `*_SUITE` modules; `make_emakefile` generates `Emakefile` from `ERL_COMPILE_FLAGS`, `tests`/`opt` compile the suites into `.` (`EBIN = .`, so beams land next to the sources).
- `ERL_COMPILE_FLAGS += -Werror +nowarn_missing_spec_documented`, then `:= $(filter-out +deterministic,...)` — test suites are deliberately NOT compiled with `+deterministic` even when the build sets `ERL_DETERMINISTIC=yes`.
- `runtime_tools.spec` — `{suites,"../runtime_tools_test",all}.` (all suites in the installed dir, discovered automatically).
- `runtime_tools.cover` — `{incl_app,runtime_tools,details}.{excl_mods, runtime_tools, [observer_backend]}.` (observer_backend is excluded from coverage).
- `release_tests_spec` installs the suites, `.spec`/`.cover`, the generated `Emakefile`, `*_SUITE_data`, plus `$(ERL_TOP)/make/otp_version_tickets` → `system_information_SUITE_data/` and `$(ERL_TOP)/otp_versions.table` → `versions_SUITE_data/otp_versions.table`.

## Test Strategy
- Run from `lib/runtime_tools`: `make test` (`TEST_NEEDS_RELEASE=true` in `../Makefile`).
- `make test` runs `$(ERL_TOP)/make/test_target_script.sh`: it builds and installs a full release into a temp root, installs the test_server harness, then runs `${RELEASE_ROOT}/bin/ct_run` — a source tree alone is not sufficient.
- Run a subset with `make ARGS="-suite dbg_SUITE -case erl_tracer" test` (ARGS is forwarded straight to `ct_run`).
- Before `ct_run`, `ts:compile_datadirs/2` builds every `*_SUITE_data` dir, so `Makefile.src` templates are substituted into `Makefile` and `make` is run there.
- Every suite uses the `ts_install_cth` Common Test hook, which is what drives `ts_lib:make_non_erlang/2` on the suite's data dir.
- Suites are integration-level where they need a fresh node: `instrument_SUITE` and parts of `dbg_SUITE` spawn peers via `?CT_PEER()`/`peer:stop/1`.

## Suite → Coverage
| Suite | Scope | What it covers |
| --- | --- | --- |
| `dbg_SUITE` | unit + integration | `dbg` local/global tracing, function & send/`'receive'` match specs, save/load patterns (`wtp`/`rtp`), `trace_port(file|ip, ...)` including the `trace_file_drv` driver atom and IP port busy handling, trace file wrapping/`wrap_port_time`, tracer exit handling, `dbg:session` semantics (parent down), and `erl_tracer` module tracers (local NIF tracer + distributed `?CT_PEER()` tracer) |
| `dyntrace_SUITE` | integration (dtrace only) | `dyntrace` front-end: enumerates probes in the running `beam*.smp` binary, runs an empty script, verifies process spawn/schedule/hibernate/exit probe output |
| `dyntrace_lttng_SUITE` | integration (LTTng only) | LTTng tracepoints (`org_erlang_dyntrace:*`) driven through the `lttng` CLI: process/port events, call/return, messages, GC, `t_all` |
| `instrument_SUITE` | integration (peer nodes) | `instrument:allocations/0,1` and `instrument:carriers/0,1`, `test_format` histogram layout, `test_abort` on `erts_internal:gather_alloc_histograms/1` / `gather_carrier_info/1`; spawned peer uses `+Meamax`/`+Meamin`, `+Muatags`, `+Muramv`, `+MBsbct 1`, `+Muacul 70` |
| `runtime_tools_SUITE` | smoke | `test_server:app_test/1` (`.app` file) and `appup_test/1` (`.appup`), plus `application:start/stop` supervision-tree shutdown |
| `system_information_SUITE` | unit + integration | `system_information:report/0`, `to_file/1`, `from_file/1` (against the stored 2013 report), server API `load_report/0,2` + `applications/environment/application/module` accessors, `sanity_check/0` |
| `scheduler_SUITE` | unit (timing sensitive) | `scheduler:sample/0,1`, `sample_all/0`, `utilization/1,2`, `get_sample/0,1`, and the interaction with the `scheduler_wall_time` system flag (incl. restoring the flag in `utilization(Seconds)`) |
| `msacc_SUITE` | unit (timing sensitive) | `msacc` microstate accounting: `start/0,1`, `stop/0`, `stats/0,1,2,3`, `to_file`/`from_file`, `print/1,2,3`, `reset/0` |
| `observer_backend_SUITE` | unit (resource heavy) | `observer_backend:procs_info/1` chunked messages (10 000 procs per chunk) and `etop_collect/1` with 20 000 spawned processes |
| `versions_SUITE` | unit | `versions` module: `check/1`, `compare/2`, `list_compare/2`, `branch/1`, `branch_base/1`, `string_to_list`/`list_to_string`, plus `ct_doctest:module(versions, ...)` over its doc examples; `valid_versions/1` parses `versions_SUITE_data/otp_versions.table` |
| `zzz_SUITE` | debug-build helper | Named to sort last; `lc_graph/1` just calls `erts_debug:lc_graph/0` to dump lock-checker graph files before the VM terminates |

## Dependencies / Fixtures
- `dbg_SUITE_data/` — `dbg_SUITE.c` + `Makefile.src` build the `dbg_SUITE` NIF shared object (via `@dll@`/`@SHLIB_RULES@` substitution in `ts_lib:make_non_erlang/2`), loaded by `dbg_SUITE:load_nif/1` for the `erl_tracer`/`distributed_erl_tracer` cases; `dbg_test.erl` is a small test module the suite compiles itself at runtime with `compile:file(...,[trace])`.
- `dbg_SUITE_data/exref_td.erl` — unused leftover fixture; nothing in the repo references it.
- `system_information_SUITE_data/information_test_report.dat` — ~500 KB stored report generated from `r16b02` (2013); `api_from_file/1` and `validate_server_interface/1` only check that the required top-level keys are present, so it is a backward-compatibility fixture for old report files.
- `system_information_SUITE_data/otp_version_tickets` — copied in at install time from `$(ERL_TOP)/make/otp_version_tickets`; read by `init_per_testcase(sanity_check, ...)`.
- `versions_SUITE_data/` — tracked only by an empty `.gitignore`; `otp_versions.table` is copied in at install time by `release_tests_spec`.
- Suites need a built release and, per case, external tools: `sudo` + `/usr/sbin/dtrace` (`dyntrace_SUITE`), the `lttng` CLI (`dyntrace_lttng_SUITE`), a working `peer` node start, and `ts_install_cth`'s `variables` file from `lib/common_test/test_server`.

## Known Issues / Gotchas
- Dynamic-tracing suites are effectively dormant on Linux/x86: `dyntrace_SUITE:all/0` skips unless `erlang:system_info(dynamic_trace) == dtrace` (Solaris/illumos) and explicitly skips `systemtap` and `lttng` as "currently not supported"; `dyntrace_lttng_SUITE:init_per_suite/1` skips unless `dynamic_trace == lttng`.
- `dyntrace_SUITE` drives a real `sudo dtrace` port and compares the exact line ordering of probe output (spawn/in/hibernate/out/in/exit/out) — fragile and environment dependent.
- `zzz_SUITE` is a no-op on release emulators: `erts_debug:lc_graph/0` returns `notsup` unless the emulator was built with `ERTS_ENABLE_LOCK_CHECK`; the ordering assumption is only that the name sorts after the other suites.
- Timing-sensitive assertions: `msacc_SUITE:api_timer/1` requires the counted 100 ms sample to be within 80 000–120 000 µs, and `api_start_stop/1` compares derived counters with a 0.5 tolerance; `scheduler_SUITE` asserts float/percent agreement within 0.0555 (see its `MaxDiff` comment) and checks that `logical_processors_available` is `undefined` when no `weighted` scheduler entry is present.
- `instrument_SUITE` is skipped under AddressSanitizer (`test_server:is_asan/0`), spawns peer nodes, deliberately churns memory (1 MB `binary_to_list`, 128-message process churn) and only asserts *lower* bounds on block/carrier counts, so `sl_alloc` verification is intentionally skipped as "inherently unstable".
- `observer_backend_SUITE` spawns 20 000 processes per case and asserts the first two `procs_info` chunks are exactly 10 000 entries long — memory-hungry and sensitive to chunking changes.
- `system_information_SUITE:sanity_check/1` is skipped when `otp_version_tickets` starts with `DEVELOPMENT` and is documented as liable to fail on non-tagged versions; the 500 KB report fixture is tied to an old report format, so format changes require regenerating it.
- `versions_SUITE:doctests/1` depends on the `versions` module doc examples staying in sync (`{missing_tests, [...]}` list must be updated when doc examples are added).

## Notes for Agents
- `test/Emakefile` and the compiled `*.beam` files here are generated/ignored (root `.gitignore` ignores `*.beam` except `lib/*/test/*_SUITE_data/*.beam`).
- All suites are discovered through `runtime_tools.spec`'s `all`, so adding a suite only requires adding it to `MODULES` in the `Makefile`.
- Test helpers (`ct_doctest`, `?CT_PEER()`, `ts_install_cth`, `test_server:app_test/1`) come from `common_test`; see `../../common_test/CONTEXT.md`.

## See Also
- `../src/` — the modules under test (`dbg`, `instrument`, `msacc`, `scheduler`, `system_information`, `versions`, `observer_backend`, `dyntrace`).
- `../../common_test/test_server/` — the legacy harness (`ts`, `ts_lib`, `ts_install_cth`) that builds data dirs and drives release tests.
- `../../../HOWTO/TESTING.md` — how to run and debug OTP tests.
