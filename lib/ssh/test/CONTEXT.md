# ssh test suites

## Intent
Common Test suites and support modules for the `ssh` application (client + daemon + SFTP + agent).
Flat directory: `*_SUITE.erl` suites, shared helper modules, per-suite `<SUITE>_data/` fixtures, and one nested `property_test/` (QuickCheck/PropEr).
Installed into the release as `$(RELEASE_PATH)/ssh_test` (`release_tests_spec` in `test/Makefile`).

## How tests are built and run
- `make` in `lib/ssh/` (or `lib/`) builds and installs the suites; `test/Makefile` sets `RELSYSDIR = $(RELEASE_PATH)/ssh_test`.
- `test/Makefile` `MODULES` lists every compiled module; `emakebuild` generates `Emakefile` via `make/make_emakefile`.
- Compile flags: `-I$(ERL_TOP)/lib/ssh/src -pa ../ebin +nowarn_missing_spec_documented`; `+deterministic` is filtered out.
- `HRL_FILES` pulls the app headers from `../src` (`ssh.hrl`, `ssh_agent.hrl`, `ssh_auth.hrl`, `ssh_connect.hrl`, `ssh_transport.hrl`, `ssh_xfer.hrl`) plus local `ssh_test_lib.hrl`.
- Running the suites is driven by the OTP test harness (`make/test_target_script.sh` → `ct_run` with a spec); `SPEC_POSTFIX` selects `ssh_<postfix>.spec`.
- Suites use `ts_install_cth` (standard OTP test-server install CT hook) and some add `{cth_events, …}` for logger-event verification.

## Spec files (which suites run)
- `ssh.spec` — full/lab run: `{suites,"../ssh_test",all}`, skips only `ssh_bench_SUITE` and `ssh_upgrade_SUITE`.
- `ssh_gh.spec` — GitHub-Actions PR gate (auto-selected via `SPEC_POSTFIX=gh`); additionally skips `ssh_renegotiate_SUITE`, `ssh_collect_labmachine_info_SUITE`, and slow cases (see skip lists).
- `ssh_dev.spec` — local developer iteration; runs only a subset of suites and skips timer-heavy cases.
- `ssh_bench.spec` — benchmarks only (`ssh_bench_SUITE`).
- `ssh_sup.spec` — focused run of `ssh_sup_SUITE`, `ssh_connection_SUITE`, `ssh_to_openssh_SUITE`.
- All specs set `{enable_builtin_hooks,false}` and `{ct_hooks,[{cth_log_redirect,[{mode,replace}]}]}`.

## Cover files
- `ssh.cover` — excludes supervisors, API behaviour modules and test/info modules (`ssh_dbg`, `ssh_info`).
- `ssh_all.cover` — whole `ssh` app, no exclusions.
- `ssh_limited.cover` — like `ssh.cover` but also excludes the supervisors.

## Test Strategy
- Suites typically `ssh:start()`, spin up an in-process daemon with `ssh_test_lib:daemon/1,2,3` (returns `{Pid,Host,Port}`) on an ephemeral port, then connect with `ssh_test_lib:connect/2,3` or `ssh:connect/3`.
- Key material and fixtures live in each suite's `<SUITE>_data/` (private+public keys, `authorized_keys`, `known_hosts`, host keys); suites copy them into `priv_dir` via `ssh_test_lib:system_dir/1` / `user_dir/1`.
- `ssh_test_lib.hrl` provides `?CHECK_CRYPTO/1` (skips when crypto is unavailable), `?wait_match/…` polling, `?TIMEOUT` (15s) and `?CT_LOG/?CT_PAL/?CT_FAIL` logging macros.
- Many suites declare a `suite()` with a `timetrap` (e.g. 15–180s) and the `ts_install_cth` hook.
- `cth_events` (`cth_events.erl`) verifies logger events per testcase via `ssh_test_lib:add_log_handler/get_log_events/analyze_events`; note it is fragile under `p_basic`-style parallel groups (see FIXME in that module).
- `ssh_cth` logs diffs of `~/.ssh/known_hosts` around suite/group/testcase boundaries; not enabled by the shipped specs.

## External / environment-dependent suites
- `ssh_to_openssh_SUITE` — Erlang↔OpenSSH interop; skips if `os:find_executable("ssh")` fails or no sshd reachable (sanity-checks with `ssh_test_lib:openssh_sanity_check`).
- `ssh_compat_SUITE` — interop against many OpenSSH/OpenSSL versions run in **docker** containers (`docker` must be on PATH); some ancient versions are explicitly `{skip,"Not tested"}`.
- `ssh_bench_SUITE` — benchmark suite (`connect`, `transfer_text`), runs 8 iterations; excluded from `ssh.spec`/`ssh_gh.spec`, run via `ssh_bench.spec`.
- `ssh_collect_labmachine_info_SUITE` — lab-only; collects machine/SSH info and transfers results, excluded from the GH gate.
- `ssh_upgrade_SUITE` — release upgrade/downgrade of the ssh app (`ct_release_test`); excluded from `ssh.spec`/`ssh_gh.spec`.
- `ssh_agent_SUITE` — needs a Unix-domain ssh-agent socket.

## property_test/ (property-based tests)
- Contains `ssh_eqc_encode_decode.erl`, `ssh_eqc_client_server.erl`, `ssh_eqc_client_info_timing.erl`, `ssh_eqc_subsys.erl` — QuickCheck/EQC modules requiring a QuickCheck license (see `property_test/README`); meant to run manually.
- Driven by `ssh_property_test_SUITE.erl` in the parent dir via `ct_property_test:init_per_suite/1` (tool auto-detected: EQC or PropEr/triq).
- `property_test/ssh_eqc_client_server_dirs/` holds the key pairs (`id_*`, `ssh_host_*_key`) used by the client/server stateful model.
- `ssh_eqc_event_handler.erl` (parent dir) is the EQC report handler used when running these.

## Routing Table
| Path / module | Purpose |
| --- | --- |
| `ssh_options_SUITE.erl`, `ssh_connection_SUITE.erl`, `ssh_protocol_SUITE.erl` | Largest suites — options parsing, connection layer, and raw transport/protocol (uses `ssh_trpt_test_lib`). |
| `ssh_basic_SUITE.erl`, `ssh_algorithms_SUITE.erl`, `ssh_renegotiate_SUITE.erl`, `ssh_pubkey_SUITE.erl` | Core client/daemon behaviour, algorithm negotiation, renegotiation, public-key auth. |
| `ssh_sftp_SUITE.erl`, `ssh_sftpd_SUITE.erl`, `ssh_sftpd_erlclient_SUITE.erl` | SFTP client API, sftpd server, and the Erlang client of sftpd. |
| `ssh_agent_SUITE.erl`, `ssh_agent_mock_server.erl`, `ssh_engine_SUITE.erl`, `ssh_sup_SUITE.erl`, `ssh_dbg_SUITE.erl`, `ssh_chan_behaviours_SUITE.erl` | ssh-agent, key engines, supervisors, debug tracing, channel behaviours. |
| `ssh_to_openssh_SUITE.erl`, `ssh_compat_SUITE.erl` | OpenSSH/docker interop (external deps). |
| `ssh_bench_SUITE.erl`, `ssh_collect_labmachine_info_SUITE.erl`, `ssh_upgrade_SUITE.erl` | Benchmark / lab-only / release-upgrade suites. |
| `property_test/` | QuickCheck (EQC/PropEr) property modules + their key dirs (see `property_test/README`). |
| `ssh_test_lib.erl`, `ssh_test_lib.hrl` | Main test helper: daemon/connect/exec/shell helpers, key-cb helpers, logging helpers, macros. |
| `ssh_trpt_test_lib.erl`, `ssh_echo_server.erl`, `ssh_bench_dev_null.erl`, `ssh_peername_sockname_server.erl`, `ssh_test_cli.erl`, `ssh_relay.erl` | Support: transport-level test scripting, echo/null/peername channel servers, CLI-driven channel, TCP relay. |
| `ssh_key_cb*.erl`, `ssh_algorithms_key_cb.erl` | In-memory client/server key callbacks used by various suites. |
| `ssh_cth.erl`, `cth_events.erl` | Common Test hooks (known_hosts logging; logger-event verification). |
| `*.cover`, `*.spec` | Cover exclusion configs and Common Test spec files. |

## Known Issues / Notes for Agents
- `*_SUITE_data/` directories hold real key material (private+public) used by the suites; treat as fixtures, not production assets.
- `ssh_sftp_SUITE_data/test_data/*` is mostly generated at runtime — `.gitignore` keeps only `big.txt`, `f2.txt`, `f1.txt`, `d1/f1`, `d1/f2`.
- `property_test/ssh_eqc_client_server_dirs/{system,user}` are generated and git-ignored.
- `cth_events` logger-event checks are unreliable under parallel testcases (documented FIXME in the module); `ssh_dev.spec` orders `ssh_basic_SUITE` first to avoid logger-event leakage between suites' daemon startups.
- Suites needing external tooling: `ssh_to_openssh_SUITE` (openssh `ssh`/`sshd`), `ssh_compat_SUITE` (docker + images), `ssh_agent_SUITE` (agent socket) — they `{skip,…}` rather than fail when unavailable.
- `ssh_bench_SUITE` uses a two-node cluster (`ts_install_cth` with `nodenames,2`) and is excluded from default full runs.
