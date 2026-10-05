# diameter — Test Suites

## Intent
Common Test suites plus shared test helpers for the `diameter` protocol stack (RFC 6733); `TEST_NEEDS_RELEASE=true` (see `../Makefile`), so the suites are compiled and run against a *released* OTP tree, not the source tree.
Every suite is additionally runnable standalone without common_test through its own `run/0` entry point (`make _<suite>`).
Tests are protocol/system level: they start real `diameter` services over TCP/SCTP/TLS and drive traffic between in-process or peer-node Diameter nodes.

## API Surface
Helper modules (compiled and released with the suites, listed in `modules.mk`):
- `diameter_util.erl` (~3000 lines, the primary helper; `diameter_util.hrl` exposes most of it via macros):
  - Suite lifecycle: `init_per_suite/1` (prints host info, computes `dia_factor`, may `{skip, …}` the whole suite, starts `diameter_event_logger`), `end_per_suite/1`.
  - Fun evaluation/monitoring: `run/1` (evaluates funs in spawned, monitored processes, collects results, raises on failure, propagates `{skip, Reason}`), `proxy_call/4`, `eprof/1`, `log/4`, `f/2`, `formated_timestamp/0`.
  - Diameter/service helpers: `listen/2,3`, `connect/3,4`, `disconnect/4`, `lport/2` (finds a listening port via `diameter_reg` internals), `info/0`, `consult/2` (reads the generated `.app` file / dictionaries).
  - Generic helpers: `mktemp/1`, `tmpdir/0`, `unique_string/0`, `choose/1`, `scramble/1`, `fold/3`, `peer/1` (starts a `peer` node with the current cookie), `have_sctp/0`, `lib_dir/2`, `name/1`.
  - ~2200 lines of the file are OS host-inspection code (`analyze_and_print_host_info/0` and per-OS branches) plus skip logic (`maybe_skip/1`, `os_cond_skip*`) — do not mistake it for usable test API.
- `diameter_util.hrl` — the macro layer used by every suite (`?LOG`, `?RUN`, `?PEER`, `?LISTEN`, `?CONNECT`, `?DISCONNECT`, `?CHOOSE`, `?SCRAMBLE`, `?HAVE_SCTP`, `?MKTEMP`, `?UNIQUE_STRING`, `?MINS/?SECS`, `?DEL_START/?DEL_REG/…`).
- `diameter_ct.erl` — Makefile entry point: `run/1` calls `ct:run_test/1` with `{logdir, "./log"}` and `{auto_compile, false}`, `cover/0` runs `./testspec`; prints time/process-count/memory deltas.
- `diameter_enum.erl` — "enumeration as a function on integers" so codec tests can step through huge cartesian products without materialising them.
- `diameter_codec_test.erl` — shared encode/decode test logic (`base/0`, `gen/2`, `lib/0`) used by the codec/dictionary suites.
- `diameter_event_logger.erl` — gen_server started by `diameter_util:init_per_suite/1`; subscribes to services registered with `?DEL_REG/1` and logs the `#diameter_event{}` stream for post-mortem, unsubscribing when the registering client dies.
- Build/spec files: `Makefile` + `modules.mk` (`MODULES`, `DATA`), `diameter.spec` / `diameter_gh.spec` (release ct specs), `testspec` (local spec used by `make cover`), `diameter.cover` + `coverspec.sed` (coverage spec, excludes `diameter_dbg/info/etcp/etcp_sup`), `depend.sed` → `depend.mk` (beam file → local include dependencies), `release.sed` (rewrites `-include("diameter*.hrl")` to `-include_lib` when releasing suites, since generated hrls are not installed in `../include`).

## Constraints
- Every suite includes `diameter_util.hrl` (and `diameter.hrl` when needed); `HRL_FILES = diameter_util.hrl`.
- Compile flags are `-I ../include -I ../src/gen` plus `+warn_export_vars +warn_unused_vars`; `+deterministic` is filtered out of `ERL_COMPILE_FLAGS` (deterministic paths break log/code inspection).
- `SUITES` is derived from `MODULES` by stripping the `diameter_` prefix and `_SUITE` suffix — the `make <suite>` / `make _<suite>` targets are generated from it, so a new suite must be named `diameter_<name>_SUITE` and added to `MODULES` (with any `*_SUITE_data` file added to `DATA` and any non-suite helper to `MODULES`) to be compiled, run and covered.
- Tests must work with `../src` absent: data is located via `?LIB_DIR(App, Dir)`/`code:lib_dir/1-2` or ct's `data_dir`/`priv_dir`, never by relative path into the source tree.
- Key material is never committed — `diameter_tls_SUITE` generates keys/certs into `priv_dir` at run time with the `openssl` CLI.
- Scenarios that contend for global `diameter` state, ports or scheduler load are not run concurrently: `diameter_3xxx_SUITE` uses ct groups with grouped (serial) execution, `diameter_tls_SUITE` deliberately has a single `test/1` case instead of a parallel one.

## Test Strategy
- Local, in-tree (fastest loop): from this directory `make <suite>` (one suite through ct), `make _<suite>` (one suite without ct), `make run` (all suites, no ct), `make ct` (all suites through ct), `make cover` (all suites + coverage); ct output is piped through `awk '/ FAILED /{rc=1}'` to derive the exit status, and `-pa $(realpath ../ebin)` is absolute because ct changes directory.
- Release-based (what `TEST_NEEDS_RELEASE=true` implies): `make test` from `../` or repo root (`make diameter_test`) runs `make/test_target_script.sh`, which releases OTP into `test/make_test_dir`, installs the suites there via `release_tests_spec`, and runs `ct_run -spec make_test_dir/diameter_test/diameter.spec`; CI (`.github/workflows/reusable-test.yaml`) sets `SPEC_POSTFIX=gh` so `diameter_gh.spec` is used, and `CTRUN_TIMEOUT=90`.
- `release_tests_spec` installs sources plus `diameter.spec`/`diameter_gh.spec`/`diameter.cover` to `$(RELEASE_PATH)/diameter_test`, tars `../examples` into `$(RELEASE_PATH)/diameter_test/examples`, and installs `DATA` files into per-suite `*_SUITE_data` dirs.
- Both specs run all suites but skip `diameter_gen_sctp_SUITE`; individual suite selection is done with `ARGS="-suite <suite>"` / `ct_run`.
- ct timetraps range from 10 s (`diameter_gen_tcp_SUITE`) to 315 s (`diameter_watchdog_SUITE`); most suites are one or few testcases that internally exercise many permutations, so a single testcase often runs for minutes.
- Runtime prerequisites: a distributed node for `diameter_dist_SUITE`/`diameter_distribution_SUITE`/`diameter_examples_SUITE` (they assert `is_alive()` and spawn `peer` nodes; the Makefile supplies `-sname diameter_test_<suite>`), SCTP support in the kernel for SCTP coverage (`?HAVE_SCTP()` gates it; some platforms/CI have none), and the `openssl` CLI for `diameter_tls_SUITE`.

## Known Issues / Notes for Agents
- `diameter_util:init_per_suite/1` classifies the host (kernel, distro, BogoMIPS, schedulers, memory, virtualisation, `TS_EXTRA_PLATFORM_LABEL`, ct timetrap scale factor) into a numeric `dia_factor` put in the suite config; a factor > 10 makes the heavy cases `{skip, {machine_too_slow, Factor}}`, so slow hosts silently lose coverage.
- `maybe_skip/1` (called from `init_per_suite/1`) skips the *whole* suite on a small set of old kernels/distros (e.g. Fedora 16, Wind River 5.0.1.0, Debian 6), and the host-info code shells out (`os:cmd`, `/proc`, `sysctl`) — on unusual hosts it may throw, in which case `init_per_suite/1` falls back to `dia_factor = 1`.
- `diameter_traffic_SUITE` (the largest suite, ~1900 lines) deliberately randomises its configuration per run (`?CHOOSE(?TRANSPORTS)`, encodings, dictionaries, senders) and expects coverage to accrue across nightly runs rather than within one run.
- Several suites need a distributed Erlang node and/or peer nodes (`diameter_dist_SUITE`, `diameter_distribution_SUITE`, `diameter_examples_SUITE`); running them with a plain `ct_run` on a non-distributed node fails rather than skips.
- SCTP-related failures: `diameter_gen_sctp_SUITE` exists to demonstrate `gen_sctp`/OS problems found during diameter development and is skipped by both release specs; SCTP also affects `diameter_transport_SUITE`, `diameter_pool_SUITE`, `diameter_traffic_SUITE`, `diameter_examples_SUITE`.
- `diameter_util:run/1` evaluates funs in parallel within the same node, so services/AVP dictionaries used by concurrent cases must have unique names (`?UNIQUE_STRING()`) and clean up transports/services themselves.
- `log/` (ct logs), `depend.mk` and `coverspec` are generated (see `.gitignore`).

## Routing Table
| Suite | Protocol area covered |
| --- | --- |
| `diameter_3xxx_SUITE.erl` | 3xxx result codes / `application_opt() request_errors`, answer and callback behaviour (ct groups, serial) |
| `diameter_app_SUITE.erl` | Self-checks of the diameter app resource file: keys, vsn, modules vs installed beams, no `export_all`, minimal release via `systools`, xref, relup |
| `diameter_capx_SUITE.erl` | Capabilities exchange (CER/CEA) between Diameter nodes, incl. error and event handling |
| `diameter_codec_SUITE.erl` | Encode/decode of dictionary-generated modules, unknown/mandatory AVP handling (uses `diameter_codec_test` and `diameter_codec_SUITE_data/`) |
| `diameter_codegen_SUITE.erl` | Dictionary compiler *code generation*: enum encode/decode picks the right module in an inheritance chain, enum empty-value calls not duplicated |
| `diameter_compiler_SUITE.erl` | Dictionary compiler error handling/formatting and line-rewriting (`format`, `replace`, `generate`, `flatten1/2` over `base_rfc3588.dia`) |
| `diameter_config_SUITE.erl` | Service and transport configuration, especially detection/reporting of config errors |
| `diameter_dist_SUITE.erl` | Traffic where the *server* is spread over several (peer) Erlang nodes |
| `diameter_distribution_SUITE.erl` | Traffic where the *client* is spread over several (peer) Erlang nodes / `share_peers` |
| `diameter_dpr_SUITE.erl` | `disconnect_cb` configuration and Disconnect-Peer-Request handling |
| `diameter_event_SUITE.erl` | Events delivered by `diameter:subscribe/1` (watchdog events are covered by `diameter_watchdog_SUITE`) |
| `diameter_examples_SUITE.erl` | The example services under `../examples/code` (also spawns a peer node) |
| `diameter_failover_SUITE.erl` | Failover/retransmission across seven Diameter nodes in four realms |
| `diameter_gen_sctp_SUITE.erl` | Non-diameter `gen_sctp` behaviour that historically broke diameter tests (skipped by both release specs) |
| `diameter_gen_tcp_SUITE.erl` | Non-diameter `gen_tcp` behaviour that can break tests in other suites |
| `diameter_indirect_inherits_SUITE.erl` | Dictionary compiler `indirect_inherits` semantics: limited vs whole-dict imports, vendor ids, grouped AVPs, enums along the chain |
| `diameter_length_SUITE.erl` | `transport_opt() length_errors` (malformed/huge message handling) |
| `diameter_pool_SUITE.erl` | `pool_size`: multiple connections between connecting peers |
| `diameter_reg_SUITE.erl` | The server implemented by `diameter_reg` |
| `diameter_relay_SUITE.erl` | Relay agent traffic across seven Diameter nodes / two realms |
| `diameter_stats_SUITE.erl` | The server implemented by `diameter_stats` |
| `diameter_sync_SUITE.erl` | The server implemented by `diameter_sync` |
| `diameter_tls_SUITE.erl` | TLS: inband security after capabilities exchange and TLS at connection establishment, six nodes (single serial `test/1`, certs generated at run time) |
| `diameter_traffic_SUITE.erl` | General client↔server traffic exercising the stack with randomised configurations (largest suite) |
| `diameter_transport_SUITE.erl` | `diameter_tcp`/`diameter_sctp` as implementations of the transport interface |
| `diameter_watchdog_SUITE.erl` | RFC 3539 watchdog state machine as implemented by `diameter_watchdog` (longest timetrap) |
| `diameter_codec_SUITE_data/` | Dictionary/Erlang inputs used by `diameter_codec_SUITE` (`DATA`, installed to `$(RELSYSDIR)`) |
| `diameter_tls_SUITE_data/` | Legacy `Makefile.ca` for manual cert generation; not in `DATA` and unused by the suite (certs are generated in `priv_dir`) |

## See Also
- `../CONTEXT.md` — the `diameter` application itself (`Makefile`, `TEST_NEEDS_RELEASE`, `DIA_PLT_APPS`, `src/`, `examples/`).
- `../../make/test_target_script.sh` — the harness behind `make test` (`TEST_NEEDS_RELEASE`, `SPEC_POSTFIX`, `ARGS`, `CTRUN_TIMEOUT`).
- `../../HOWTO/TESTING.md` — general OTP test/release workflow (`./otp_build tests`, `$TESTROOT`).
