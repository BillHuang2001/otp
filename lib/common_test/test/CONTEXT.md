# common_test — Own Test Suite (`common_test_test`)

## Intent
The test suite of the `common_test` application: it verifies the CT framework itself (test-run semantics, hooks, test specs, groups, logging, cover, surefire, master, interactive shell).
The dominant pattern is **meta-testing**: an outer CT suite starts a *nested* CT run on a separate node against a small dummy suite in a sibling `*_SUITE_data/` directory, then asserts on the emitted events, the log files and the returned run result.
The directory doubles as the app's `test/` dir (`make test` with `TEST_NEEDS_RELEASE=true`) and as the release-installed `$(RELEASE_PATH)/common_test_test`, which is the target of `common_test.spec`.

## API Surface — the shared meta-test helpers
- `ct_test_support.erl` — helper library used by nearly every suite; imported/aliased by suites directly.
  - Standard callbacks: `init_per_suite/1,2`, `end_per_suite/1`, `init_per_testcase/2`, `end_per_testcase/2`.
  - `init_per_suite/1` starts the nested-run node; `end_per_suite/1` deletes its code path entry and stops it.
  - `get_opts/1` builds the nested run's base options: `{logdir, …}` plus `ct_opts` from Config plus `-ct_test_vars` from the outer node.
  - `run/2,3,4` runs the nested suite **twice** — once via `ct:run_test/1` (rpc to the CT node) and once via `ct_run:script_start/0` — and `check_result/3` asserts both produced a consistent ok/failed/skipped result.
  - `start_event_receiver/1` + `get_events/2` collect the nested run's event stream on the CT node.
  - `verify_events/3,4` matches that stream against the expected event list; `reformat/2` normalises it first and `log_events/4` dumps it to `<testcase>.events` in `priv_dir`.
  - Utilities: `write_testspec/2,3`, `ct_rpc/2`, `wait_for_ct_stop/1`, `random_error/1`, `unique_timestamp/0`, `rm_dir/1`, `start_slave/3`, `slave_stop/1`.
- `ct_test_support_eh.erl` — `gen_event` handler (aliased as `?eh`) installed in the nested run by the `{event_handler,{ct_test_support_eh,[{cbm,ct_test_support},{trace_level,Level}]}}` option; it forwards every `#event{}` to `ct_test_support:handle_event/2`.
- `test_server_test_lib.erl` + `test_server_test_lib.hrl` — CT hook that starts a `test_server_tester@Host` slave node for `test_server_SUITE`/`erl2html2_SUITE`; also `parse_suite/1`, which parses a legacy `suite.log` into `#suite{}`/`#tc{}` records.
- `telnet_server.erl` — minimal local telnet server (`start/1`/`stop/1`, port 1234) spawned per test case by `ct_telnet_SUITE`.
- `ct_userconfig_callback.erl` — config callback module used by the user-config test cases.
- `common_test.spec`, `common_test_gh.spec`, `common_test.cover` — app-level run specs and the coverage config, installed together with the suites.

## The meta-testing pattern
- The nested run must live on a **separate node** (`ct@Host`, started by `ct_test_support:start_slave/3`) because CT's own processes (`ct_util_server`, log handlers) would clash with the outer run.
- `start_slave/3` puts `priv_dir`, the parent of `data_dir` and the dir of `ct_test_support` on the CT node's code path, optionally starts `sasl`, and adds `{ct_node,CTNode}` / `{ct_opts,…}` to Config; if `<data_dir>/ct.trace` exists it switches to `{ct_trace,File}` with `trace_level` 0.
- Canonical test case: `DataDir = ?config(data_dir, Config)` → build `Opts` (`{suite,Dir}` | `{dir,Dir}` | `{spec,File}` plus an `eh_opts`-style event-handler option) → `start_event_receiver` → `run(Opts,Config)` → `get_events` → `verify_events(events_to_check(Case), Events, Config)`.
- Expected events are declared per test case as `{?eh, EventName, Data}` lists; `verify_events1/3` walks the observed stream in order and `locate/4` understands `parallel`/`shuffle` group wrappers, `{negative,NoMatch,Match}`, `'$proplist'`, `'_'` wildcards, funs and `{'DEF','RUNDIR'|'LOGDIR'|'START_TIME'|'STOP_TIME'}` placeholders.
- Each nested run is labelled (`{label,Case}`) so several runs from one test case are distinguishable in the log dir.
- Test cases that assert on log content read the log through `?config(priv_dir,Config)` (e.g. `ct_log_SUITE` re-reads the previous test case's `tc_logfile` via `save_config`).

## `*_SUITE_data/` directories (44 of them)
- Each holds the mini-suite(s) the outer suite runs: one or more `*_SUITE.erl` (often nested as `<name>/test/<name>_SUITE.erl` to mimic CT's dir layout), plus `.cfg`/`.xml`/`.conf` config files, `.spec` test specifications and small helper modules (`cth_*.erl` hooks, event handlers, protocol/dummy modules).
- Nothing here is compiled by `test/Makefile`; the mini-suites are compiled by CT at run time (auto-compile) or loaded from the nested node's code path.
- `property_test/ct_prop.erl` follows the `ct_property_test` convention: a `property_test/` subdirectory of the suite's data dir containing the property modules.

## Constraints
- Suites must not depend on being run from the source tree: all paths come from `Config` (`data_dir`, `priv_dir`, `ct_node`, `trace_level`, `logdir`), and at release time the sources, `*.spec`, `common_test.cover` and every `*_SUITE_data`/`property_test` dir are copied/tarred into `$(RELEASE_PATH)/common_test_test`.
- `lib/common_test/Makefile` sets `TEST_NEEDS_RELEASE=true`: `make test` builds a release and runs these suites from the installed `common_test_test` dir, not in place.
- Every suite and helper module must be listed in `Makefile`'s `MODULES` to be compiled and installed.
- Compilation is in place (`EBIN = .`, `erl -make` with the generated/git-ignored `Emakefile`); the resulting `.beam` files are git-ignored and must never be committed (except under `*_SUITE_data/`, which the root `.gitignore` un-ignores).
- Non-Erlang data files need a sibling `.license` file (e.g. `ct_doctest_SUITE_data/doctest_fail.md.license`) for the SPDX/license scanner.
- Capability-dependent suites self-skip instead of failing: `ct_netconfc_SUITE` (no ssh/crypto), `ct_property_test_SUITE` ("No property testing tool found" unless `eqc`/`proper`/`triq` is installed), `ct_release_test_SUITE` (Windows), `ct_telnet_SUITE`'s `unix_telnet` case (needs a `testconn` telnet `require` from `ts.unix.config`), cover suites when the outer run already runs cover.
- Several suites start extra nodes and are slow: `ct_master_SUITE` starts 5 nodes and writes master logs to `/tmp` (or `c:/Temp` on Windows, then tars them into `priv_dir`), `ct_cover_SUITE`/`ct_cover_nomerge_SUITE` use `ct_slave`, `ct_hooks_SUITE` and `ct_error_SUITE` are the largest suites.

## Test Strategy
- This directory *is* the test strategy for `common_test`: run `make test` in `lib/common_test`, or `ct_run -spec common_test.spec` from the installed `common_test_test` dir.
- `common_test.cover` = `{incl_app,common_test,details}`: the whole app is instrumented for cover.
- `common_test.spec` runs `{suites,"../common_test_test",all}`; `common_test_gh.spec` is the CI variant and skips `ct_release_test_SUITE` ("Versions not always correct") — that is the only suite excluded from GitHub Actions CI.
- Behaviour-changing env vars: `CT_TEST_OPTS` (override nested-run options; overrides `get_opts/1` results), `CT_USE_TMP_DIR` + `TMP`/`TEMP` (put nested log dirs under tmp), `CT_SAVE_OLD_LOGS` (retention seconds for `ct_run*` dirs deleted by `ct_test_support`).
- Coverage gaps: nothing exercises the GUI-ish parts of CT; the telnet (`unix_telnet`), SNMP and netconf paths only run where the environment supplies the external tool; `ct_release_test` and `ct_property_test` are effectively skipped in most environments.

## Routing Table — suite groups (by area under test; representative, not exhaustive)
| Area | Suites |
| --- | --- |
| Nested-run basics / smoke | `ct_smoke_test_SUITE`, `ct_basic_html_SUITE`, `ct_auto_compile_SUITE`, `ct_auto_clean_SUITE`, `ct_misc_1_SUITE`, `ct_system_error_SUITE`, `ct_group_leader_SUITE` |
| Groups & ordering | `ct_groups_test_1_SUITE`, `ct_groups_test_2_SUITE`, `ct_groups_spec_SUITE`, `ct_groups_search_SUITE`, `ct_group_info_SUITE`, `ct_sequence_1_SUITE` |
| Test specifications | `ct_testspec_1_SUITE`, `ct_testspec_2_SUITE`, `ct_testspec_3_SUITE`, `ct_config_SUITE` |
| Repeat | `ct_repeat_1_SUITE`, `ct_repeat_testrun_SUITE`, `ct_tc_repeat_SUITE` |
| Logging & IO | `ct_log_SUITE`, `ct_pre_post_test_io_SUITE`, `ct_verbosity_SUITE`, `ct_unicode_SUITE`, `ct_keep_logs_SUITE`, `ct_util_SUITE` |
| Config / info / skip / errors | `ct_config_info_SUITE`, `ct_priv_dir_SUITE`, `ct_skip_SUITE`, `ct_error_SUITE` |
| Hooks | `ct_hooks_SUITE` |
| Events & event handlers | `ct_event_handler_SUITE` |
| Framework interfaces | `ct_test_server_if_1_SUITE`, `ct_SUITE` (app/appup consistency) |
| Cover | `ct_cover_SUITE`, `ct_cover_nomerge_SUITE` |
| Reporting | `ct_surefire_SUITE`, `erl2html2_SUITE` |
| Master / distribution / connections | `ct_master_SUITE`, `ct_gen_conn_SUITE`, `ct_telnet_SUITE`, `ct_snmp_SUITE`, `ct_netconfc_SUITE` |
| Release upgrade | `ct_release_test_SUITE` |
| Interactive shell | `ct_shell_SUITE` |
| Doctests | `ct_doctest_SUITE` (calls `ct_doctest:module/1` directly on compiled fixtures) |
| Property testing | `ct_property_test_SUITE` + `property_test/ct_prop.erl` (calls `ct_property_test:quickcheck/2`) |
| Legacy Test Server | `test_server_SUITE` (runs `test_server_ctrl` on a remote node, parses `suite.log`) |
| Shared helpers | `ct_test_support.erl`, `ct_test_support_eh.erl`, `test_server_test_lib.erl`, `telnet_server.erl`, `ct_userconfig_callback.erl` |

## Notes for Agents
- To add a suite: create `ct_<x>_SUITE.erl`, add it to `MODULES` in `Makefile`, and for a nested-run suite copy the `init_per_suite`/`end_per_suite`/`init_per_testcase`/`end_per_testcase` boilerplate plus the `get_opts` → `start_event_receiver` → `run` → `get_events` → `verify_events` flow.
- `?eh` is `ct_test_support_eh`; expected events are declared as `{?eh,EventName,Data}` and matched structurally, so a change in CT's events requires updating both `events_to_check/1` and the underlying data suite.
- Four suites do *not* use `ct_test_support` and test their target module directly in-process or on a remote node: `ct_SUITE`, `erl2html2_SUITE`, `test_server_SUITE` (via the `test_server_test_lib` hook), and `ct_doctest_SUITE`; `ct_property_test_SUITE` only uses `ct_property_test` itself.
- `suite()` in essentially every suite installs the OTP-wide `ts_install_cth` hook, which lives in `../test_server/` and is not part of CT proper.

## See Also
- `../src/` — the implementation under test (`ct`, `ct_run`, `ct_util`, `test_server`, `cth_*` hooks, `ct_netconfc`/`ct_telnet`/`ct_snmp`, `ct_doctest`, `ct_property_test`, `ct_cover`, `ct_slave`).
- `../test_server/` — legacy Test Server framework and the `ts_install_cth` CT hook used by suite definitions.
- `../proper_ext/` — property-test tool adapters (`ct_quickcheck_ext`, `ct_proper_ext`) exercised indirectly by `ct_property_test_SUITE`.
- `../include/`, `../priv/`, `../doc/` — public headers, runtime data and documentation of the app.
- `../CONTEXT.md` — the `lib/common_test` node; `../../lib/CONTEXT.md` and repo-root `CONTEXT.md` for build/release entry points.
