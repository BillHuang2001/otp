# kernel — Common Test suites

## Intent
Common Test suite directory for the OTP `kernel` application, covering everything in `../src/`: application/code control, files, disk_log, distribution, sockets/inet, logger, os/terminal, seq_trace.
The directory is FLAT: ~62 `*_SUITE.erl` suites, ~35 cross-suite helper modules, a set of helper test applications (`*.erl` + `.app`), and `*_SUITE_data/` input directories.
Suites run against an INSTALLED OTP release (`TEST_NEEDS_RELEASE=true` in `../Makefile`), never directly against the source tree.

## Suites by area
- Application/init/startup control: `application_SUITE` (app controller: load/start/stop/restart, config, takeover/failover, `distr_changed`), `kernel_SUITE` (kernel app + appup version sanity), `kernel_config_SUITE` (kernel config sync, `start_distribution=false`), `init_SUITE` (init args, boot scripts, restart/stop status, `-S`/`-run` flags), `erl_boot_server_SUITE` (boot server API), `erl_prim_loader_SUITE` (prim loader incl. primary archive), `heart_SUITE` (heart port program), `group_SUITE` (io `group` module), `error_handler_SUITE` (`'$handle_undefined_function'`), `zzz_SUITE` (must run last; dumps the lock-check graph).
- Code & loading: `code_SUITE` (code server: paths, archives, sticky dirs, `on_load`), `code_coverage_SUITE` (coverage-mode APIs), `multi_load_SUITE` (`code:ensure_modules_loaded` atomic multi-load).
- Distribution & inter-node: `erl_distribution_SUITE` (net_kernel/dist setup, cookies, node naming, `monitor_nodes`), `erl_distribution_wb_SUITE` (white-box dist dflags/switch options), `global_SUITE` (global registry + locks), `global_group_SUITE` (connected/visible node groups), `net_SUITE` (the `net` module API), `erpc_SUITE` (incl. calls against old releases), `rpc_SUITE`, `pg_SUITE` (incl. old-release and netsplit groups).
- Files & logs: `file_SUITE`, `file_name_SUITE`, `prim_file_SUITE`, `ram_file_SUITE`, `zlib_SUITE`, `disk_log_SUITE`, `wrap_log_reader_SUITE`.
- Sockets & inet (largest area): `inet_SUITE`, `inet_res_SUITE`, `inet_sockopt_SUITE`, `gen_tcp_api_SUITE`, `gen_tcp_echo_SUITE`, `gen_tcp_misc_SUITE`, `gen_udp_SUITE`, `gen_sctp_SUITE`, `sendfile_SUITE`, `prim_net_SUITE`, `gen_tcp_socket_SUITE`, `socket_SUITE`, `socket_api_SUITE`, `socket_sctp_SUITE`, `socket_sctp_example_SUITE`, `socket_traffic_SUITE`, `socket_ttest_SUITE`, plus `socket_suites` (wrapper suite that runs all socket suites in one ct run).
- `gen_tcp_api`/`gen_tcp_echo`/`gen_tcp_misc`/`gen_udp`/`inet_sockopt`/`gen_tcp_socket` share an `inet_backend_default` / `inet_backend_inet` / `inet_backend_socket` group scheme (see `kernel_test_lib:inet_backend_opts/1`).
- Logger: `logger_SUITE`, `logger_std_h_SUITE`, `logger_disk_log_h_SUITE`, `logger_env_var_SUITE`, `logger_formatter_SUITE`, `logger_filters_SUITE`, `logger_legacy_SUITE`, `logger_olp_SUITE`, `logger_proxy_SUITE`, `logger_simple_h_SUITE`, `logger_stress_SUITE` (benchmark-only), `error_logger_SUITE`, `error_logger_warn_SUITE`.
- OS/terminal/misc: `os_SUITE`, `interactive_shell_SUITE` (`to_erl`/`tty`/`ssh` groups), `standard_error_SUITE`, `seq_trace_SUITE`, `bif_SUITE`, `pdict_SUITE`.

## Helper modules
- `kernel_test_lib` (+`kernel_test_lib.hrl`) — the central helper, used by nearly every suite: `init_per_suite` host analysis/auto-skip, `start_node`/`stop_node`, socket `open`/`listen`/`connect` wrappers, inet-backend selection, `tc_try`, `?P` logging macros.
- `kernel_test_sys_monitor`, `kernel_test_global_sys_monitor` — watchdog processes that flag misbehaving nodes during a suite run.
- `shell_test_lib` (+`shell_test_lib.hrl`) and `rtnode` — drive a real tty/`run_erl` shell via `tmux` for `interactive_shell_SUITE`.
- `logger_test_lib` — starts a peer node with a generated kernel config to inspect logger configuration.
- `ignore_cores` — Common Test hook that redirects/ignores core files around test cases.
- `cleanup` — standalone module (not a `*_SUITE`) that disconnects leftover distributed nodes.
- Socket helpers: `socket_test_lib` (+`.hrl`, `?SLIB`, bench events), `socket_test_logger`, `socket_test_evaluator` (+`.hrl`) with `socket_test_ev_proxy` (cross-node "evaluator" coordination), `socket_test_profile` (fprof wrapper).
- Ttest (throughput) framework: `socket_test_ttest_lib`, `socket_test_ttest.hrl`, `socket_test_ttest_client.hrl`, and the `socket_test_ttest_tcp_{gen,gs,socket}` × `{client,client_gen,client_gs,client_socket,server,server_gen,server_gs,server_socket}` matrix.
- SCTP traffic helpers: `socket_sctp_traffic_lib` (+`.hrl`), `socket_sctp_traffic_client`, `socket_sctp_traffic_server`.
- Distribution test backends: `test_inet_tcp`, `test_inet6_tcp`, `test_inet_udp`, `test_inet6_udp`, `block_select_dist`, plus generated `gen_tcp_dist.erl`/`erl_uds_dist.erl` copies from `../examples`.
- Test applications (each with a matching `.app`): `myApp`, `topApp`, `topApp2`, `topApp3`, `appinc1`, `appinc1x`, `appinc2`, `appinc2A`, `appinc2B`, `appinc2top`, `ch` + `ch_sup`.
- `code_a_test` (file whose module is named `code_b_test`) and `code_b_test` — mismatched-module tests for `code_SUITE`.

## Spec and cover files
- `kernel.spec` — all suites except `logger_stress_SUITE`; `kernel_gh.spec` — CI spec (skips `logger_stress_SUITE` plus docker-broken `global_SUITE` cases), selected by `SPEC_POSTFIX=gh` in `.github/workflows/reusable-test.yaml`.
- `kernel_smoke.spec` — a handful of fast inet/inet_res/gen_tcp/heart cases; `kernel_bench.spec` — bench groups plus `logger_stress_SUITE`; `logger.spec` — the logger suites only.
- Spec suite paths are relative (`"../kernel_test"`) because `ct_run` runs with cwd `make_test_dir/kernel_test`.
- `kernel.cover` (whole app), `logger.cover` (logger modules), `tty.cover` (`prim_tty`, `user_drv`, `group`, `shell`, `edlin`, `io_lib`) are the `COVERFILE` list for coverage runs.

## Build, install and test strategy
- Run from `lib/kernel`: `make test` → `make/test_target_script.sh`; with `TEST_NEEDS_RELEASE=true` it releases OTP into `make_test_dir/Erlang ∅⊤℞`, runs `make RELEASE_PATH=.../make_test_dir release_tests_spec`, then `ct_run -spec <spec>` from `.../make_test_dir/kernel_test` using the released `ct_run`.
- `release_tests_spec` installs `ERL_FILES`, `HRL_FILES`, `APP_FILES`, the spec/cover files, every `*_SUITE_data/` dir and `../examples` into `$(RELEASE_PATH)/kernel_test`; anything not listed there is unavailable at test time.
- `*_SUITE_data` dirs carrying a `Makefile.src` (C helpers, NIFs, drivers) are compiled by `ts:compile_datadirs` before the run; `gen_tcp_api_SUITE_data/gen_tcp_api_SUITE.c` is loaded as a NIF at runtime.
- Suites declare `{ct_hooks,[ts_install_cth]}` (from `../../common_test/test_server`), which adapts paths for release-based runs; extra ct_run args can be passed via `EXTRA_ARGS`, and `TYPE`/`FLAVOR` select the emulator type/flavor.
- Suites routinely spawn peer nodes with `?CT_PEER` / `kernel_test_lib:start_node`, using `?UNIQ_NODE_NAME` to avoid name clashes.
- Heavy/slow suites: `socket_api_SUITE` (~28k lines), `socket_SUITE`, `gen_tcp_misc_SUITE` (~10k lines), `socket_ttest_SUITE`/`socket_traffic_SUITE` (`bench`/`tbench` groups, runtime tunable with `ESOCK_TEST_*` env vars), `file_SUITE`/`disk_log_SUITE`, `logger_stress_SUITE` (excluded by the default specs), `interactive_shell_SUITE` (needs `tmux` ≥ 3 and ssh keys), `erl_distribution_SUITE`, `global_SUITE`, `application_SUITE`.

## Constraints
- Every compiled module must be listed in this dir's `Makefile` (`MODULES`, `SOCKET_MODULES`, `ERL_FILES`, `HRL_FILES`, `APP_FILES`); unlisted sources are neither compiled nor installed, so suites cannot use them at runtime.
- Keep the directory flat: a new suite or helper belongs at top level; a subdirectory is only added for a `*_SUITE_data/` input dir or a self-contained helper area such as `esock_misc/`/`esock_ttest/`.
- Per-suite inputs must live in `<SUITE>_data/` so Common Test exposes them through `data_dir`; there is no cross-suite data sharing.
- A new `*.spec` variant must also be added to the `release_tests_spec` install list before it can be used against an installed release.
- `EBIN = .`, so compiled beams land in this directory; `*.beam` and `Makefile`-generated `Emakefile` are covered by the root `.gitignore`.
- This directory owns tests only — never edit `../src/` from here.

## Known Issues
- `socket_sctp_SUITE.erl` contains a literal NUL byte (~offset 210436, inside a format string), so `rg`/`grep` treat it as binary and need `-a`/`--text`; it still compiles.
- `gen_tcp_dist.erl` and `erl_uds_dist.erl` here are copies generated from `../examples/` (and ignored by the root `.gitignore`) — edit the `../examples` sources, not the copies.
- `esock_misc/` and `esock_ttest/` are manual developer tools: not installed into the release and not referenced by any suite or Makefile.
- `cleanup.erl` does not match the `*_SUITE` naming convention, so the suite spec never runs it automatically.
- `global_group_SUITE_data/` is intentionally empty (kept so Common Test still provides a `data_dir`).
- Many suites self-skip depending on host resources (SCTP support, `tmux`, ssh, a local name server, non-loopback interfaces); check skips before treating a run as a regression.
- Some suites are host-sensitive by design (`kernel_test_lib` host analysis, `logger_test_lib`), so absolute timing/print expectations can differ per machine.

## Notes for Agents
- `make` in this directory only compiles the suites; running tests requires a built/installed OTP (`make test` in `lib/kernel`).
- `socket_*` suites are also reachable individually through the `socket_suites` wrapper, which runs them as groups (`main`, `api`, `traffic`, `ttest`, `gen_tcp*`, `gen_udp`) and forces the socket inet-backend for the `gen_*` groups.
- Several suites are >300 KB and `socket_api_SUITE.erl` exceeds 1 MB — read with offset/limit or grep for `all()`/`groups()` instead of reading in full.
- Keep `zzz_SUITE` last alphabetically (its comment states it must run after everything else) and keep suite names ending in `_SUITE`.
- `kernel_TEST` hosts analysis keys (`kernel_factor`, host labels) come from `kernel_test_lib` and appear in the suite config.

## Routing Table
- `esock_misc/` → manual, non-installed esocket/iow test clients and servers (developer tools)
- `esock_ttest/` → manual escript/sh drivers for the ttest throughput framework (developer tools)
- `application_SUITE_data/` → test apps, `t*.config`, `deadlock/`, `subdir/` for `application_SUITE`
- `code_SUITE_data/` → fake lib roots, `dummy_app-*`, archives, `on_load*` fixtures for `code_SUITE`
- `disk_log_SUITE_data/` → pre-existing disk_log/halt-log files for `disk_log_SUITE`
- `erl_prim_loader_SUITE_data/` → `primary_archive/` for `erl_prim_loader_SUITE`
- `erpc_SUITE_data/` → `fwd_node.c` (old-release forwarding helper) for `erpc_SUITE`
- `file_SUITE_data/` → compression/tar fixtures for `file_SUITE`
- `gen_tcp_api_SUITE_data/` → `gen_tcp_api_SUITE.c` NIF (raw-fd helpers) for `gen_tcp_api_SUITE`
- `global_SUITE_data/` → `global_trace.erl` helper for `global_SUITE`
- `global_group_SUITE_data/` → empty placeholder dir
- `heart_SUITE_data/` → `simple_echo.c` compiled test program for `heart_SUITE`
- `inet_SUITE_data/` → `hosts`/`resolv.conf` fixtures for `inet_SUITE`
- `inet_res_SUITE_data/` → `otptest` zone data and `run-ns` (local name server) for `inet_res_SUITE`
- `inet_sockopt_SUITE_data/` → `sockopt_helper.c` for `inet_sockopt_SUITE`
- `interactive_shell_SUITE_data/` → keymap configs, `io_columns/io_rows.erl`, `ssh_host_rsa_key` for `interactive_shell_SUITE`
- `os_SUITE_data/` → C helpers (`my_echo.c`, `my_fds.c`, `sys_shell.c`) plus `unix/` and `win32/` trees for `os_SUITE`
- `prim_file_SUITE_data/` → compression fixtures for `prim_file_SUITE`
- `ram_file_SUITE_data/` → `.gz`/`.uu` fixtures for `ram_file_SUITE`
- `seq_trace_SUITE_data/` → `echo_drv.c` linked-in driver for `seq_trace_SUITE`
- `wrap_log_reader_SUITE_data/` → `wrap_log_test.erl` + `Makefile.src` for `wrap_log_reader_SUITE`
- `zlib_SUITE_data/` → zip/gzip/zlib fixtures for `zlib_SUITE`

## See Also
- `../src/` → the kernel implementation under test (read-only from here; escalate source changes to `lib/kernel`).
- `../Makefile` → the owning app Makefile; sets `TEST_NEEDS_RELEASE=true`.
- `../../make/test_target_script.sh` → the `make test` driver (release build + `ct_run`).
- `../../common_test/test_server/ts_install_cth.erl` → the hook every suite declares via `{ct_hooks,[ts_install_cth]}`.
- `../../examples/gen_tcp_dist/`, `../../examples/erl_uds_dist/` → sources copied in as `gen_tcp_dist.erl`/`erl_uds_dist.erl`.
- `../../erts/emulator/test/` → emulator-side socket/esock tests that complement the `socket_*` suites here.
