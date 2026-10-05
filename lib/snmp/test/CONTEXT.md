# snmp — Test Suite (`snmp_test`)

## Intent
Common Test suite directory of the OTP **snmp** application (agent + manager + MIB compiler + auxiliary APIs).
All suites are driven by OTP's `test_server`/Common Test framework (`ts_install_cth` hook); at release time the whole directory is installed as `$(RELEASE_PATH)/snmp_test` and driven by `snmp.spec`.
Tests exercise the *installed* snmp application, so a built release/app must exist before running them.

## API Surface
### Build / driver files
- `modules.mk` — authoritative file lists: `SUITE_MODULES`, `TEST_UTIL_MODULES`, `MODULES`, `HRL_FILES`, `MIB_FILES`, `NETSNMP_MIB_FILES`, `COMPILER_MIB_FILES`, `SPECS`. Add new suites/helpers here.
- `Makefile` — compiles the `.erl` (via generated `Emakefile`, `EBIN = .`) and MIB files, and runs tests.
- `snmp_test_server.erl` — lightweight standalone test server (`run(Suite)`), used by the `test` target instead of full Common Test.
- `snmp_test_suite.erl` — declares the (legacy, optional) suite behaviour used by `snmp_test_server`.
- `snmp.spec` — `{suites,"../snmp_test",all}` selectors for the OTP test runner; `snmp.cover` — coverage spec (`incl_app,snmp`, excludes snmp_index, error-report/behaviour modules, `snmpm_user`, network-interface behaviours).

### Suites (`*_SUITE`)
- `snmp_agent_SUITE` — the main agent suite (very large): v1/v2c/v3 request processing, mib-storage (ets/dets/mnesia), multi-threaded, multiple-request, informs, standard MIBs, reported bugs and OTP tickets (also `inet_backend_{default,inet,socket}` groups).
- `snmp_agent_mibs_SUITE` — agent mib-server/mib-storage behaviour incl. `size_check` group per storage backend.
- `snmp_agent_conf_SUITE` — agent config files (community, usm, vacm) reload/validation.
- `snmp_agent_nfilter_SUITE` — agent network-interface filter (notification filtering) callbacks.
- `snmp_app_SUITE` — application resource file / appup (upgrade) checks; uses `snmp_appup_mgr` + `snmp_appup_test`.
- `snmp_compiler_SUITE` — the MIB compiler (`snmpc`) incl. error/warning handling and ticket cases.
- `snmp_conf_SUITE` — general (system-level) snmp configuration handling.
- `snmp_log_SUITE` — log handling: open/write/close, log_to_io, log_to_txt.
- `snmp_manager_SUITE` — the main manager suite: start/stop, notifications, requests (get/get-next/set/bulk), events, USM/AES, IPv6, v3, tickets.
- `snmp_manager_config_SUITE` — manager runtime configuration (users, agents, usm users, counters, stats).
- `snmp_manager_user_SUITE` — manager user registration/unregistration and crash handling.
- `snmp_note_store_SUITE` — manager notification store.
- `snmp_pdus_SUITE` — PDU encode/decode.
- `snmp_to_snmpnet_SUITE` — interoperability with external Net-SNMP tools (see `snmp_to_snmpnet_SUITE_data/`).

### Shared test libraries (in `TEST_UTIL_MODULES`)
- `snmp_test_lib` + `snmp_test_lib.hrl` — the base helper: macros `?LIB`, `?TC_TRY`, `?SKIP`/`?FAIL`, timing, node helpers, print helpers, suite init/end hooks, `fix_data_dir/1`.
- `snmp_agent_test_lib` — agent test helpers (start v1/v2/v3/mt/bilingual agents, config writers, MIB load/unload, request helpers, `snmp_agent_test_get`).
- `snmp_test_manager` — a manager implemented as a `gen_server` used by tests.
- `snmp_test_mgr`, `snmp_test_mgr_misc`, `snmp_test_mgr_counter_server` — simple agent-suite manager and its counter/stat helpers.
- `snmp_manager_user`, `snmp_manager_user_old`, `snmp_manager_user_test_lib`, `snmp_otp16649_user`, `snmp_appup_mgr` — manager-user callback implementations used by the manager/user/appup suites.
- `snmp_test_sys_monitor`, `snmp_test_global_sys_monitor` — OS process/port leak monitors started per suite.
- MIB instrumentation modules loaded by the test MIBs: `sa`, `klas3`, `test1`, `test2`.

## Constraints
- MIB fixtures must be compiled to `.bin`/`.hrl` before suites run; `make mib` (or `make`) does this using the snmp MIB compiler from `../ebin`.
- Adding a suite/helper requires listing it in `modules.mk` (`SUITE_MODULES`/`TEST_UTIL_MODULES`); suites are picked up via `Modules = $(MODULES)` and the `_'*_SUITE_make'` Emakefile glob.
- Compiled `.beam` files, the generated `Emakefile` and generated MIB artifacts are build output — never edited by hand (`EBIN = .` puts beams in this directory).
- Suite compilation uses `-Werror` by default and `+warn_unused_vars` (override with `SNMP_WARNING_AS_ERROR=false` / `WARN_UNUSED_VARS=false`), the `sys_pre_attributes` parse transform, and defines `-Dsnmp_test_data=snmp_test_data`, `-Dversion=...`.
- Test code includes snmp internals via `-I../src/{app,misc,agent,manager}` and `-I../include`; `-include("snmp_test_lib.hrl")` is the standard prelude.

## Test Strategy
- `make test` (in this dir) runs the two legacy targets: `agent` (`snmp_test_server run snmp_agent_SUITE`) and `manager` (`snmp_test_server run snmp_manager_SUITE`), each as a non-shell node with its own `-sname`.
- `make mib` compiles MIB fixtures; `make` / `make targets` compiles the Erlang modules; `make info` prints all effective flags.
- Individual suites are normally run through Common Test (`ct_run`/`test_server`); the suite-level `ts_install_cth` hook provides the test-case expansion used on OTP's build machines.
- Useful make variables: `SNMP_DEBUG=d` (enable `?DBG` tracing via `-Dsnmp_debug`), `DONT_USE_TS=true` (compile with `-DDONT_USE_TEST_SERVER`: use `snmp_test_lib` timing/watchdog instead of `test_server`), `DONT_STOP=true` (omit `-s init stop` so the node stays alive after a run for interactive inspection), `DISABLE_TC_TIMEOUT=true` (adds `-snmp_test_timeout`), `SNMP_DESC=true` (MIB compiler `+{description,true}`), `MIBS_VERBOSITY=<n>` (MIB compiler verbosity), `MERL=<erl>` (override the emulator used to run tests).
- `make release_tests_spec` installs the suites, `snmp.spec`, `snmp.cover` and the `*_SUITE_data`/`snmp_test_data` fixture dirs into `$(RELEASE_PATH)/snmp_test`.

## Known Issues
- `snmp_to_snmpnet_SUITE` needs external Net-SNMP programs (`snmpget`, `snmpd`, `snmptrapd`); it is skipped where they are absent (see its data dir CONTEXT.md), so interop coverage is thin on most CI hosts.
- `snmp_agent_SUITE` and `snmp_manager_SUITE` are the largest files in the app (both >200 KB); avoid opening them whole — grep for the case name first.
- `test-mibs/` is a legacy manual MIB corpus, not wired into the build/tests.

## Notes for Agents
- `snmp_test_data/` MIBs listed in `MIB_FILES` (`modules.mk`) are precompiled by `Makefile` (with explicit ordering deps Klas4←Klas3, SA-MIB←OLD-SNMPEA-MIB, Test3←Test2); suites load the resulting `.bin`/`.hrl`.
- `snmp_test_lib:fix_data_dir/1` plus `-Dsnmp_test_data` redirect a suite's `data_dir` to this directory's `snmp_test_data/`; other `*_SUITE_data/` dirs follow the normal Common Test convention.
- Suites share the `?LIB` macros from `snmp_test_lib.hrl` — use them instead of raw `test_server` calls so `DONT_USE_TS` builds keep working.
- `snmp_test_sys_monitor:start()`/`stop()` are expected in `init_per_suite`/`end_per_suite` to catch leaked OS processes/ports.

## Routing Table
- `snmp_test_data/` → shared + precompiled MIB fixtures for the suites (most suites read MIBs from here)
- `snmp_to_snmpnet_SUITE_data/` → Net-SNMP interop fixtures for `snmp_to_snmpnet_SUITE` (external `snmpget`/`snmpd`/`snmptrapd`)
- `test-mibs/` → legacy manual MIB corpus incl. `*-error.mib` negative cases (not build-wired)
- `test_config/` → agent/manager/system config templates (`*.src`) expanded by its own Makefile
- `../` (`lib/snmp/`) → the application under test (`src/agent`, `src/manager`, `src/misc`, `mibs/`, `priv/mibs`)
- `../test/` test helper `snmp_test_lib` — see `../CONTEXT.md`

## See Also
- `../CONTEXT.md` — the snmp application itself (agent, manager, MIB compiler, includes).
- `../../common_test/` — the framework the suites are written for.
- `../../erts/` — `ts_install_cth`/`test_server` test drivers live with the OTP test infrastructure.
