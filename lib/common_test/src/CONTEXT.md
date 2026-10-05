# Common Test — Erlang Sources

All Erlang sources of the OTP `common_test` application (vsn `1.31.2`, see `../vsn.mk`).
This directory has no subdirectories; modules are grouped below.

## Intent
Common Test (CT) is the test framework OTP uses to test every other application in the repository.
This node holds the whole CT runtime: the documented `ct` user API, the `ct_run` command-line driver, the suite/testcase execution engine, logging/reporting, the hook framework, test-spec parsing, and the connectivity helper libraries suites use to drive external systems (telnet/ftp/ssh/snmp/netconf).
The `test_server*` modules are the historical core of the framework and keep their old names; they are framework-agnostic (see Execution Model).

## Build
- `Makefile` compiles all modules into `../ebin`; `BEHAVIOUR_MODULES = ct_suite ct_hooks`, everything else is in `MODULES`.
- `common_test.app.src` (vsn `%VSN%`; `applications = [kernel,stdlib]`) → `../ebin/common_test.app`; `common_test.appup.src` is the release-upgrade template. Both are `sed`-templated from `../vsn.mk`.
- `ERL_COMPILE_FLAGS` = `-pa ../ebin -I../include -I $(ERL_TOP)/lib/snmp/include -I../../xmerl/inc -I $(ERL_TOP)/lib/kernel/include -Werror +warn_obsolete_bool_op +nowarn_deprecated_catch` — all sources must stay warning-free.
- `runtime_dependencies` (compiler, crypto, debugger, ftp, inets, observer, runtime_tools, sasl, snmp, ssh, syntax_tools, tools, xmerl) are *optional* runtime apps, NOT hard dependencies.
- Headers in this dir — `ct_util.hrl`, `test_server_internal.hrl`, `ct_netconfc.hrl` — are internal and are NOT installed to a release.
  The public headers live in the sibling `../include/` (`ct.hrl`, `ct_event.hrl`, `ct_property_test.hrl`).

## Execution Model
- Users enter through the `ct` API (Erlang) or the `ct_run` OS command; both funnel into the `ct_run` module.
- The `bin/ct_run` executable is built from `erts/etc/common/ct_run.c` (part of `erts/`, NOT this directory): it starts `erl -s ct_run script_start` (also `-shell` / `-vts` modes).
  `ct_run:script_start/0` reads `init:get_arguments/0`, merges any `erl_flags`, filters `-erl_args` and parses the result into the `#opts{}` record.
- Flow: `ct:run/1..3 | run_test/1 | run_testspec/1` → `ct_run:run_test/1..` (`run_test1/run_dir/run_prepared`) → `ct_util:start(LogDir, Verbosity, Stylesheet)` → `ct_util:do_start/5`.
- `ct_util:do_start/5` registers the process as `ct_util_server` and starts, in order: `ct_event` (shared event manager), `ct_config` (registered `ct_config_server`), user `event_handler`s, `ct_default_gl`, `ct_logs`, and `ct_hooks`.
- There is no OTP `supervisor` in this app; `ct_util_server` and `ct_logs` are hand-rolled `spawn_link`/receive loops, and `test_server_ctrl`, `ct_config_server` are `gen_server`s. `ct_util:stop/1` and `test_server_ctrl:stop/0` tear the run down.
- `ct_run:do_run_test/3` then starts `test_server_ctrl:start_link(local)` (registered `test_server_ctrl`) and feeds it the test selection (`add_dir(_with_skip)/add_module/add_case/add_cases/add_conf/add_spec`), applies run options (`multiply_timetraps`, `scale_timetraps`, `create_priv_dir`, `reject_io_reqs`), and finishes with `catch test_server_ctrl:wait_finish()`.
- `test_server_ctrl` is the central driver: it compiles suites (`ct_make`), builds the run schedule (sequential, parallel-per-testsuite or parallel-per-testcase) via `do_test_cases/do_spec`, starts/collects target nodes (`test_server_node`), and delegates all suite semantics to `ct_framework`.
- A testcase runs inside `test_server` (the low-level API a suite actually calls; `ct:*` helpers are re-exported/delegated there).
  `test_server` is *framework-agnostic*: it dispatches `init_tc`/`end_tc`/`get_suite`/etc. through `test_server_sup:framework_call/2,3,4` to the module named by the OS env var `TEST_SERVER_FRAMEWORK`.
  `ct_run:do_run/4` sets `TEST_SERVER_FRAMEWORK=ct_framework` and `TEST_SERVER_FRAMEWORK_NAME=common_test`; if the var is `false`/`"undefined"` the `framework_call` returns its default instead of failing.
- `test_server` also runs on remote/target nodes and manages the node's group leader through `test_server_gl` and `test_server_io`.

## Module Groups

### (a) Front-end / user API
- `ct.erl` — the documented, supported user API and the module suites call: entry points `install/1, run/1..3, run_test/1, run_testspec/1, step/3,4, start_interactive/0, stop_interactive/0` (all thin delegates to `ct_run` / `ct_util`); suite API `require/1,2, get_config/1..3, reload_config/1, log/1..5, print/1..5, pal/1..5, comment/1,2, fail/1,2, testcases/2, userdata/2,3, timetrap/1, sleep/1, capture_*, notify/2, sync_notify/2, break/1,2, continue/0,1, make_priv_dir/0, set/get_verbosity`; misc `add_config/remove_config, get_status/0, abort_current_testcase/1, get_event_mgr_ref/0, get_testspec_terms/0,1, encrypt_config_file/2,3, decrypt_config_file/2,3, get_target_name/1, get_progname/0, parse_table/1, listenv/1, remaining_test_procs/0`.
  `log/print/pal` delegate to `ct_logs`; `get_config/require` to `ct_config`; the rest to `ct_util`/`test_server_ctrl`.
  `ct.erl` owns the exported types `config_key/0, target_name/0, key_or_name/0, handle/0` and `conn_log_option(s)/0, conn_log_type/0, conn_log_mod/0`.
- `ct_run.erl` — command-line/`-s ct_run script_start` entry and option parser, the `#opts{}` record, and the whole "prepare then drive" layer: `install/1,2`, `run/1..3`, `run_test/1,2`, `run_testspec/1`, `step/3,4`, `script_start/0,1`, `run_dir/run_prepared/run_one_spec/run_all_specs`, `do_run/2,3,4`, `do_run_test/3`, `compile_and_run/4`, `run_make/...`, cover/trace helpers. Also `variables_file_name/1` (used by `ct_util:read_opts/0` to find `variables.ctc` in the log dir).
- `ct_framework.erl` — the CT implementation of the pluggable `test_server` framework callbacks and the suite engine: `init_tc/3, end_tc/3,4, get_suite/1,2, get_all_cases/1, report/2, warn/1, error_notification/4, get_log_dir/0, get_logopts/0, format_comment/1, get_html_wrapper/4, error_in_suite/1, init_per_suite/1, end_per_suite/1, init_per_group/2, end_per_group/2`.
  It resolves `all/0` + `groups/0` + `group/1` into an ordered testcase list (with `ct_groups`), applies hooks around every `init_*`/`end_*`/testcase, and keeps the per-testcase statistics.
- `ct_suite.erl` — public *behaviour* module: declares the `ct_suite` callbacks (`all/0, groups/0, suite/0, group/1, init_per_suite/1, end_per_suite/1, init_per_group/2, end_per_group/2, init_per_testcase/2, end_per_testcase/2, 'Testcase'/0,1`) and the `ct_*` types suites use.

### (b) Execution-engine internals
- `test_server_ctrl.erl` — biggest module (~5.9k lines); `gen_server` registered as `test_server_ctrl`, the central run driver.
  Runs a whole run in one process: test collection (`add_*`), compilation (`ct_make`), scheduling of sequential/parallel suites and testcases, per-testcase invocation of `test_server`/`ct_framework`, timetrap scaling, cover hooks, target-node handling, and `wait_finish/idle_notify/abort`.
  Also exposes the shared low-level helpers other modules call: `print/2,3,4, format/1,2,3, to_string/1, escape_chars/1, uri_encode/1,2, xhtml/2, get_target_info/0, get_hosts/0, node_started/1, start/stop_node/1,3, cover/1,2,3, cover_compile/7, cover_analyse/2, cross_cover_analyse/2`, and the debugger interface `i/0, p/1,3, pi/2,4, t/0,1`.
- `test_server.erl` — the low-level runtime API suites call (`?config`, IO, timetrap, `?t` macros, `init_tc/end_tc` dispatch via `framework_call`, target-node/group-leader setup, `get_loc`, `timetrap_scale_factor`). Starts `test_server_break_process[_<Str>]` for `ct:break/1` with `release_shell`.
- `test_server_sup.erl` — despite the name it is NOT a supervisor; it is a collection of support functions: `timetrap/2,3,4`, `timetrap_cancel/1`, `capture_get/1`, `messages_get/1`, `timecall/3`, `call_crash/5`, `app_test/2`, `appup_test/1`, crash-dump handling (`crash_dump_dir/0, check_new_crash_dumps/0, tar_crash_dumps/0, cleanup_crash_dumps/0`), host/username helpers, `framework_call/2,3,4`, `format_loc/1`, `unique_name/0`, `call_trace/1`.
- `test_server_io.erl` — IO server (`gen_server`) that multiplexes the group leaders/fds of a test node; `start_transaction/end_transaction`, `print_buffered/1`, `set_gl_props/1`, `set_footer/1`.
- `test_server_gl.erl` — group-leader process for a test node (captures IO, escapes HTML, forwards to `test_server_io`/`ct_logs`).
- `test_server_node.erl` — starts, monitors and shuts down Erlang target/slave nodes for `test_server_ctrl`; looks up `ts.config`/host info via `test_server_ctrl:get_hosts/0`.
- `test_server_internal.hrl` — internal records `#target_info{}` and `#par{}`, `priv_dir`, `MAIN_PORT`.
- `ct_util.erl` — the run-state server. `start/0,1,3,4` (registered `ct_util_server`) plus the ETS tables it owns: `ct_attributes` (`?attr_table`), `ct_connections`, `ct_boards`, `ct_suite_data`, `ct_verbosity_table` (all named public ETS).
  API: testdata/suite-data get/set/update, connection registry (`register_connection`, `get_connection`, `silence_connections`, ...), verbosity, cwd control (`set_cwd/reset_cwd/get_start_dir`), process marking (`mark_process/is_marked/remaining_test_procs`), profile data, `get_mode/0`, `read_opts/0`, `parse_table/1`, `listenv/1`.
- `ct_groups.erl` — parses/validates `groups/0` and `group/1` definitions and resolves group paths to testcase/repeat specs: `find_groups/4, make_all_conf/3,4, make_conf/5, delete_subs/2, expand_groups/3, search_and_override/3`.
- `ct_repeat.erl` — implements the `repeat`/`duration`/`until`/`force_stop` options: `loop_test/2` reruns the whole run and `abort/0` stops it.
- `ct_make.erl` — compiles suites/support modules and builds `-include` paths for a run (uses the `ct_make_ref` registered process for parallel compile scheduling).
- `ct_rpc.erl` — convenience wrapper around `rpc` with timeouts/error mapping: `app_node/2,3,4`, `call/4,5,6`, `cast/4,5`.
- `ct_default_gl.erl` — minimal group leader replacement (registered/conventional) so IO is never lost.
- `ct_gen_conn.erl` — generic connection-handler framework: a `gen_server` behaviour implementation a connection module (e.g. `unix_telnet`) plugs into; `start/4, stop/1, get_conn_pid/1, call/2,3, return/2, log/3, start_log/1, cont_log/2, end_log/0, make_opts/1`.

### (c) Test specifications
- `ct_testspec.erl` — parses `.spec` / `.testspec` files and `ct:run_testspec/1` terms into the internal `#testspec{}` record (also defined in `ct_util.hrl`): `prepare_tests/1,2`, `collect_tests_from_list/2,3`, `collect_tests_from_file/2,3`, `get_tests/1`, `testspec_rec2list/1,2`.

### (d) Configuration (`ct_config_callback` data)
- `ct_config.erl` — configuration server (registered `ct_config_server`, `gen_server`); `start/1, stop/0, read_config_files/1, require/1,2, get_config/1..3, get_all_config/0, set_default_config/2,3, delete_default_config/1, reload_config/1, update_config/2, release_allocated/0, add_config/remove_config, encrypt/decrypt_config_file/2,3`.
- `ct_config_plain.erl` — the default `ct_config_callback` implementation: plain `.ctc` (Erlang term) config files.
- `ct_config_xml.erl` — alternative callback reading XML config files (parsed with `xmerl`).

### (e) Logging / reporting
- `ct_logs.erl` — the logging engine and `ct_logs` registered process: builds `suite.log`/`index.html`/`TestLogs*` dirs, per-testcase HTML, statistics accumulation, `tc_log/5, tc_print/4`, (re)generation of the HTML log tree.
- `erl2html2.erl` — syntax-highlights Erlang source/terms into HTML; used for source listings in logs (calls `test_server_ctrl:uri_encode/1`).
- `ct_event.erl` — `gen_event` manager registered `ct_event`, the CT-wide event bus (`?CT_EVMGR_REF`); `add_handler/1,2, notify/1, sync_notify/1, sync_notify/2`.
- `ct_conn_log_h.erl` — event handler attached to `ct_event` that writes the "CT connection log" (telnet/ftp/ssh/netconf traffic) into the testcase HTML log; used by `cth_conn_log`.
- `ct_default_gl.erl` — see (b); its output also feeds the log.
- Master-side logging (used only by `ct_master`): `ct_master_logs.erl` (registered `ct_master_logs`), `ct_master_status.erl` (`gen_event` handler for status), `ct_master_event.erl` (`gen_event` manager `ct_master_event`, `?CT_MEVMGR_REF`).

### (f) Hooks
- `ct_hooks.erl` — public *behaviour* module (`ct_hooks`) declaring the hook callbacks (`id/1, init/2, pre/post_init_per_suite/group/testcase, pre/post_end_per_suite/group/testcase, on_tc_fail/4, on_tc_skip/4, post_groups/2, post_all/3, terminate/1`) **and** the hook-processing driver invoked by `ct_framework`/`ct_util` (`init/1`, `init_tc/3, end_tc/5`, `post_all/...`).
  Hook order is controlled by `ct_hooks_order` (`config` | `test`); built-in hooks are added by `enable_builtin_hooks`.
- `ct_hooks_lock.erl` — `gen_server` serializing access to hook processing (one hook runs at a time per hook/id): `start/1, stop/1, request/0, release/0`; used from `ct_hooks`.
- Shipped hooks: `cth_log_redirect.erl` (redirects `error_logger`/standard IO of started apps into the testcase log), `cth_conn_log.erl` (logs telnet/ftp/ssh/netconf traffic, implemented on top of `ct_conn_log_h` + the connection modules' `start_log`), `cth_surefire.erl` (writes JUnit-XML report files).
  All three depend on `ct_hooks` at build time (see `Makefile` dependencies) and are `ct_hooks` behaviours.

### (g) Distributed / ct_master
- `ct_master.erl` — orchestration of multiple `ct_run` nodes: `run/2,3, run_testspec/1,2, run_testcase/4, run_make/...`; starts `ct_master_event`/`ct_master_logs`, splits a testspec across nodes and merges the results into a master HTML index.
  `ct_master` itself does not execute testcases.
- `ct_slave.erl` — starts/stops support nodes (`start/1,2,3, stop/1,2, ...`), optionally via `ssh` (`application:ensure_all_started(ssh)`).

### (h) Connectivity / helper libraries for suites
- `ct_telnet.erl` — telnet client API over `ct_gen_conn`/`ct_telnet_client`; `open/1..4, close/1, cmd/2,3, cmdf/3,4, get_data/1, send/2,3, sendf/3,4, expect/2,3`; types `connection/0, connection_type/0, handle/0, prompt_regexp/0`.
- `ct_telnet_client.erl` — low-level telnet protocol client (option negotiation, `open/2..6, send_data/2,3, get_data/1`).
- `unix_telnet.erl` — callback module implementing the `unix_telnet` connection for `ct_telnet` (login/prompt/`?CONN_KEY` handling); the documented example of `ct_gen_conn` usage.
- `ct_ftp.erl` — FTP client helper, deprecated as a whole (the `ftp` application is going away): `open/1, close/1, cd/2, ls/2, type/2, get/3, put/3, delete/2, recv/2,3, send/2,3`; types `connection/0, handle/0`; compiled with `nowarn_deprecated_function`.
- `ct_ssh.erl` — SSH/SFTP helper wrapping the `ssh` application: connection API `connect/1,2,3, disconnect/1, session_open/1,2, shell/2,3, exec/2,3,4, subsystem/3,4, send/3,4,5, receive_response/2,3,4, send_and_receive/3..6`, plus `sftp_connect/1` and the full SFTP file-op set (`read_file/2,3, write_file/3,4, list_dir/2,3, open/3,4, close/2,3, read/3,4, write/3,4, rename/3,4, delete/2,3, make_dir/2,3, read_file_info/2,3, ...`).
- `ct_snmp.erl` — SNMP helper for suites (start/stop agent+manager, `get_next_values`, MIB loading); exports agent/manager config types; calls `application:load(snmp)` lazily.
- `ct_netconfc.erl` — RFC 6241/6242 NETCONF client (SSH or plain socket transport); the largest helper (~2.8k lines), with its own `ct_netconfc.hrl` and types (`client/0`, `session/0`, `netconf_db/0`, ...).
- `ct_gen_conn.erl` — see (b); the generic connection-handler machinery these helpers build on.

### (i) Optional feature integrations
- `ct_cover.erl` — integration with the `cover` tool: `get_spec/1, add_nodes/1, remove_nodes/1, cross_cover_analyse/2`; driven from `ct_run`/`test_server_ctrl:cover*`.
- `ct_property_test.erl` — support for QuickCheck-style property tests in suites (`init_per_suite/1, quickcheck/2,3,4, init_tool/1, present_result/4, title/1, print_frequency...`); requires a property-testing tool (Quviq QuickCheck or `proper`), so it is opt-in.
- `ct_doctest.erl` — runs doctests (shell-style `N>` blocks) found in module documentation or in files; `module/1,2,3, file/1,2,3, ...`.
- `ct_release_test.erl` — framework for testing release upgrade/downgrade of a real OTP release: starts an old OTP under a separate node (`ct_release_test-otp-<vsn>`) and drives `upgrade_init/2, upgrade_upgraded/2, upgrade_downgraded/2` callbacks; exports type `ct_data/0`.
- `cte_track.erl` — `gen_event` handler tracking CT execution progress (prints/writes a progress log); added via the `{event_handler, {cte_track, []}}` test-spec option.

## Constraints
- `common_test` must be usable to test every other OTP application: the only hard application dependencies are `kernel` and `stdlib`.
  Anything else (`crypto`, `ssh`, `snmp`, `inets`/`ftp`, `observer`/`debugger`, `tools`, `xmerl`, `sasl`, ...) must be loaded/used lazily and tolerate absence — the established patterns are `application:load/1` (`ct_snmp`, `ct_release_test`, `ct_logs`), `code:ensure_loaded/1` (`ct_telnet`), `code:which/1` checks (`ct_run`, `ct_framework`, `test_server_ctrl`), `catch`/`try` around external calls, and `framework_call(..., DefaultReturn)`.
- All sources compile with `-Werror`; adding an unused variable/warning breaks the build.
- Do not change the names or arities of the documented `ct`, `ct_suite`, `ct_hooks`, `ct_*` helper APIs without updating the docs in `../doc/`.
- Internal headers (`ct_util.hrl`, `test_server_internal.hrl`, `ct_netconfc.hrl`) must not be included from outside this application; only `../include/*.hrl` are public.
- The `#testspec{}` record and `#conn{}`/`#conn_log{}` records live in `ct_util.hrl` and are shared by many modules — changes ripple widely.
- A new module must be added to the `MODULES` (or `BEHAVIOUR_MODULES`) list in `Makefile` *and* to the `modules` list in `common_test.app.src`.

## Known Issues / Gotchas
- `test_server_ctrl` is the *legacy* name of the process that actually drives a run; `ct_run` is the modern entry point. Do not confuse `test_server_ctrl` (driver, local node) with `test_server` (per-testcase API, also on target nodes).
- `ct_util_server`/`ct_util` is not a `gen_server`; it is a hand-rolled registered process with no supervisor, so a crash of `ct_util_server` takes the whole run down and `ct_util:stop/1` is the only orderly shutdown.
- `test_server_sup` is misnamed — it is a plain function library, not a supervisor.
- Framework indirection: behaviour of a testcase depends on the `TEST_SERVER_FRAMEWORK` OS env var; when unset the `framework_call`s silently return defaults (no `init_tc`/`end_tc` hooks run). Code reading `test_server` alone will not find the CT implementation — that is `ct_framework`.
- Several modules carry `nowarn_possibly_unsafe_function`/`nowarn_deprecated_function` compile options (e.g. `test_server_sup`, `ct_cover`, `ct_netconfc`, `ct`) — these are deliberate, not accidental.
- Many-to-many coupling around the run: `test_server_ctrl`, `ct_logs`, `ct_util`, `ct_framework` and `test_server` call each other in both directions (e.g. `test_server_ctrl:escape_chars/format/print` are used by almost every module) — a "clean" module split does not exist here.

## Notes for Agents
- Before editing anything, note that `ct_doctest.erl`, `ct_property_test.erl` and `ct_netconfc.erl` contain large `-moduledoc` blocks with embedded `-module(...)` lines as doctest examples (e.g. `ct_doctest.erl:278`, `ct_property_test.erl:47,70`); these are documentation samples, not real modules.
- Change to the `ct_run`/`ct` CLI surface: option parsing lives in `ct_run.erl` (`#opts{}` + `opts2args/merge_arguments`), the OS-level wrapper in `erts/etc/common/ct_run.c`.
- The event bus (`ct_event`) and the events themselves (`?event{}` from `../include/ct_event.hrl`) are the supported way to observe a run; `cte_track`, `cth_log_redirect`, `ct_master_status` are examples.
- Suites are NOT run by invoking `test_server_ctrl` directly in development: run them through the `test/` suites of this application (see Test Strategy) which drive the public `ct` API.
- `../test_server/` (sibling dir) contains `ts.erl`/`ts_*.erl`, the "TS" frontend OTP itself uses to run its suites across the tree; it is unrelated code that also drives CT.

## Test Strategy
- Run `make test` in `../` (`lib/common_test`); the suites live in `../test/` (`*_SUITE.erl`, listed in `../test/Makefile`) and are installed as `common_test_test`.
- `../test/` covers the framework itself, driving it through the public `ct` API and `ct_run` (`ct_hooks_SUITE`, `ct_netconfc_SUITE`, `ct_doctest_SUITE`, `ct_release_test_SUITE`, ...); `ct_release_test_SUITE` is skipped in `../test/common_test_gh.spec` because OTP versions vary.
- Several suites need an SSH daemon, a NETCONF server or a running release, so failures are often environmental (missing `sshd`, missing `snmp`/`inets`) rather than code bugs.

## See Also
- `../include/` — public headers (`ct.hrl`, `ct_event.hrl`, `ct_property_test.hrl`); read-only, writes escalate to `../`.
- `../doc/` — user documentation and `docs.exs`; `../doc/notes.md` is the release notes for CT.
- `../test/` and `../test_server/` — test suites and the TS harness for this application (read-only, escalate to `../`).
- `../../xmerl/` and `../../snmp/` — include dirs referenced by the compile flags; `../../tools/` (cover), `../../ssh/`, `../../ssl/`, `../../crypto/` are optional runtime dependencies exercised here.
- `../../../erts/etc/common/ct_run.c` — the C `ct_run` launcher (owned by `erts/`).
