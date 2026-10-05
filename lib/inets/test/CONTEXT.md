# inets — Common Test Suite

## Intent
Common Test suite for the `inets` application: HTTP client (`httpc`), HTTP server (`httpd`), the shared HTTP/URI helpers and the application/supervision layer.
Everything in this directory is test-only code; it is NOT part of the app's build (`lib/inets/Makefile` has `SUB_DIRECTORIES = src examples doc`) and is exercised only through the OTP test harness (`make test`, `ct_run -spec inets.spec`).

## API Surface
### Suites by area
- HTTP client (`../src/http_client`): `httpc_SUITE.erl` (largest suite, ~4k lines, 13 groups), `httpc_cookie_SUITE.erl`, `httpc_proxy_SUITE.erl`.
- HTTP server (`../src/http_server`): `httpd_SUITE.erl` (~3.5k lines, biggest server suite), `httpd_basic_SUITE.erl`, `httpd_serve_SUITE.erl` (the `httpd:serve/1` / `erl -S httpd serve` API), `httpd_bench_SUITE.erl` (benchmark only).
- Shared HTTP/URI library (`../src/http_lib`): `http_format_SUITE.erl` (unit tests of `http_chunk`, `http_request`, `http_util`), `uri_SUITE.erl` (`http_uri:encode/decode`, string + binary).
- Application / supervision level (`../src/inets_app`): `inets_SUITE.erl` (app + appup file checks via `test_server:app_test/appup_test`, `inets:start/stop`, service start/stop, `services`/`services_info`/`service_names`), `inets_sup_SUITE.erl` (supervision tree: `default_tree`, `httpd_config`, `httpd_subtree`, `httpd_subtree_profile`, `httpc_subtree`).
- Privileged socket handling: `inets_socketwrap_SUITE.erl` (`start_httpd_fd`) — starts a separate distributed node through the setuid socket wrapper to test httpd listening on a pre-opened fd.

### httpc_SUITE groups
- `http`, `http_ipv6`, `https` do "real" requests against a locally started `inets:start(httpd, ...)` instance configured with many `mod_*` modules.
- `sim_http`, `sim_http_process_leak`, `sim_http_ipv6`, `sim_https`, `sim_mixed`, `http_unix_socket` use the in-Erlang fake server `http_test_lib:dummy_server/3,4`.
- `http_internal` exercises `mod_esi` (`erl_script_alias`) through the client; `misc` covers URI/formatting/option edge cases; `http_high_load` / `https_high_load` use `http_test_lib:open_big_data_socket/3` with `max_connections_open` and only run on 64-bit.

### Helper modules (not suites)
- `inets_test_lib.erl` (`-compile(export_all)`, ~745 lines) — the main helper: `start_apps/1`, `stop_apps/1`, `ensure_started/1`, `ensure_loaded/1`, `start_http_server/1,2`, `start_http_server_ssl/1,2`, `connect*/send/close` for socket types `ip_comm | ssl | openssl_port`, `has_ipv6_support/0,1`, `print_system_info/0,1`, `start_node/1`+`stop_node/1`, `copy_file(s)`, `copy_dirs`, `del_dirs`, `os_based_skip/1`, `non_pc_tc_maybe_skip/3`, `inet_port/1` (free-port allocator via a remote `gen_tcp:listen(0, ...)`), and TLS material generation: `hardcode_rsa_key/1` (6 fixed RSA keys), `gen_pem_config_files/3`, `der_to_pem/2`, `cert_entry/1`, `key_entry/1`, `ca_entries/1`, `extension/1`.
- `http_test_lib.erl` — the in-test fake HTTP(S)/unix-socket server (`dummy_server/3,4` with a `content_cb` implemented by the suite) and `open_big_data_socket/3` for high-load tests.
- `httpd_test_lib.erl` — server-side request/response verifier used by the `httpd_*` suites: `verify_request/6,7,8`, `verify_request_N/9`, header/status/body validation, `is_expect/1`, `port_handle_http_msg/2` callback.
- `httpd_1_1.erl` / `httpd_1_0.erl` — HTTP/1.1 and HTTP/1.0 request generators (`host`, `chunked`, `expect`, `range`, `if_test`, `trace`, `head`, cgi/esi cases) imported by `httpd_SUITE`.
- `httpd_load.erl` (`load_test/5`, `load_test_client/8`), `httpd_poll.erl` (a `gen_server` poller), `httpd_time_test.erl` (timing drivers `t/3`, `do/*`) — load/polling/timing helpers for `httpd_SUITE`.
- `httpd_bench_certs.erl` — certificate generation helpers for `httpd_bench_SUITE` (shares the hardcoded RSA keys with `inets_test_lib`).

### Headers
- `inets_test_lib.hrl` is local to this directory (defines `?ENSURE_STARTED/1`).
- `http_internal.hrl`, `httpc_internal.hrl`, `inets_internal.hrl` are **symlinks** into the sources: `../src/http_lib/http_internal.hrl`, `../src/http_client/httpc_internal.hrl`, `../src/inets_app/inets_internal.hrl` — there is no duplicated copy; edit the `src/` header.
- Suites also include them via `-include_lib("inets/src/...")` in some places (e.g. `inets_test_lib`).

### Common Test configuration files
- `inets.spec` — the default spec used by `make test`: `{suites,"../inets_test", all}` (cwd is the installed `inets_test` dir), plus legacy `cte_track` event handler, `{enable_builtin_hooks,false}` and the `cth_log_redirect` hook (`{mode, replace}`).
- `inets_bench.spec` — runs only `httpd_bench_SUITE`; not part of the default spec.
- `inets.cover` — `{incl_app,inets,details}` coverage spec for `ct_run -cover`.
- `inets.config` — legacy single-entry CT config (`{inets,[{services,[{httpd,"/ldisk/tests/bmk/inets/priv_dir/8099.conf"}]}]}`); the path is a hardcoded old test-host priv_dir and no current suite depends on it.

### Data directories
- `httpd_SUITE_data/` — `cgi_echo.c` + `Makefile.src`, `mime_types.txt`, and a full `server_root/` tree (`auth/`, `cgi-bin/`, `config/`, `htdocs/`, `icons/`) that suites copy into `priv_dir/server_root`.
- `httpd_test_data/` — a legacy near-duplicate `server_root/` tree (`conf/`, `logs/`, `ssl/`, `Makefile`, Apache docs/icons); it is only listed in the Makefile's data-dir install list, is never passed to a suite as CT `data_dir`, and is excluded from the license scanner (see repo `.ort.yml` and `scripts/license-header.es`).
- `httpd_basic_SUITE_data/` — `cgi_sleep.c` + `Makefile.src`, symlinks to `printenv.sh/.bat`.
- `httpd_serve_SUITE_data/` — small docroot (`index.html`, `sample_file.html`, `aaa/`, `subdir/`) served by `httpd:serve/1`.
- `httpd_bench_SUITE_data/` — `1k_file`, `1M_file` payloads for the benchmarks.
- `httpc_SUITE_data/` — `dummy.html`, `empty.html`, `redirect.html`, `mime.types`, SSL test PEMs, symlinks to `httpd_SUITE_data/cgi_echo.c` and `Makefile.src`.
- `httpc_proxy_SUITE_data/` — `server_proxy.sh` (starts apache2/tinyproxy/ncat for the local-proxy groups), `dummy_proxy.sh`, and an `apache2/` config + docroot.
- `inets_socketwrap_SUITE_data/` — `setuid_socket_wrap.c` + `Makefile.src`: the C socket wrapper that lets an unprivileged node bind port 80; built at test time, needs a C compiler and setuid support.
- `inets_SUITE_data/` — empty placeholder (only an empty `.gitignore`) to keep the dir in git.
- `property_test/` — contains only a `README` (QuickCheck-licensed, "run manually"); the FTP property suite was removed when ftp support was dropped from inets, so nothing in this directory is active.

## Constraints
- Test-only code: never wire this directory into `lib/inets/Makefile`'s `SUB_DIRECTORIES`.
- Requires a built `../../inets/ebin` on the code path (the Makefile adds `-pa ../../inets/ebin`) plus an installed/compiled `common_test` test server.
- The `*_internal.hrl` files must stay symlinks into `src/`; do not replace them with copies.
- Data dirs whose C sources must be built are compiled by the harness (`ts:compile_datadirs/2` over `*_SUITE_data`, using each `Makefile.src`); keep `Makefile.src` (not `Makefile`) as the tracked template.
- Suites rely on `inets_test_lib`/`http_test_lib` for setup; do not start `httpd`/`httpc` directly in new suites without stopping `inets` first (`inets_test_lib:stop_apps([inets])`).

## Known Issues
- All HTTP suites bind real TCP ports on the local interfaces; port clashes and privileged/fixed ports are the main source of flakiness (`httpd_serve_SUITE` default-port cases run `[sequence]`, `inets_socketwrap_SUITE` needs port 80, `slowdose`/high-load cases are long-running).
- `inets_socketwrap_SUITE` skips unless `os:type() == {unix, linux}`, the node is distributed (`-sname`/`-name`), and the wrapper can be run setuid — it fails with `{skip, needs_distributed_node}` / `linux_feature` otherwise.
- TLS-dependent groups (`https`, `sim_https`, `sim_mixed`, all `https_*` httpd groups) skip when `crypto` cannot be started and need working `public_key`/`ssl` and OpenSSL; certificates are generated at runtime from hardcoded RSA keys.
- IPv6 cases skip when the host has no usable IPv6 address (`is_ipv6_supported/0`, `inet_test_lib:has_ipv6_support`, or `{require, ipv6_hosts}` config); `sim_http_ipv6` excludes `server_closing_connection_on_second_response`.
- Windows: unix-domain-socket cases are skipped, CGI helpers are named `*.bat`/`*.exe` instead of `*.sh`/binary, and path handling differs — the suites branch on `os:type()` when building CGI names.
- `httpd_bench_SUITE` skips on Windows and when `nginx`/`wget` are not in `PATH`; it is excluded from `inets.spec` and must be run with `inets_bench.spec`.
- `httpc_proxy_SUITE` local/remote proxy groups depend on shell scripts and `ncat`/apache2/tinyproxy being available (`{skip, "Ncat not available on the system"}` etc.), i.e. effectively Unix-only.
- `httpd_test_data/` and `property_test/` are dead weight: nothing references them from any suite.
- `inets.config` points at a long-gone test-host path and is effectively stale.

## Test Strategy
- Full app run: `cd lib/inets && make test` — this calls `make/test_target_script.sh`, which runs `make release_tests_spec` into `make_test_dir/inets_test`, compiles data-dir C sources via `ts:compile_datadirs(..., "*_SUITE_data")`, and then runs `ct_run -spec make_test_dir/inets_test/inets.spec` with cwd set to that installed test dir.
- Per-suite runs: `ct_run -spec inets.spec` (or `-suite httpc_SUITE`), or from an Erlang shell `ct:run("../inets_test", httpc_SUITE)` as noted in the suite headers (`ts:run(inets, httpc_proxy_SUITE, [batch])` for the old harness).
- Coverage: `ct_run -cover inets.cover` in addition to the spec.
- Benchmarks: `ct_run -spec inets_bench.spec` (needs nginx/wget; expect long runtimes).
- Known coverage gaps: `mod_actions` has no dedicated assertions (`httpd_SUITE` has a commented-out `actions` case; `mod_actions` is only enabled in `httpc_SUITE`'s server config), the `heavy` load profiles in `httpd_SUITE:load/0` are disabled, and no property-based tests are active (`property_test/` dormant).
- Debug knobs while developing: the Makefile's `INETS_DEBUG` (`l`/`d`), `MANV`/`REQV`/`ACCV`/`AUTHV`/`SECV` verbosity flags and `TESTROOT_DIR`/`INETS_DATA_DIR`/`INETS_PRIV_DIR` for local runs.

## Routing Table
| Path | Area covered | Source of truth |
| --- | --- | --- |
| `httpc_SUITE.erl` | `httpc` client: methods, headers, async/stream/pipeline, keep-alive, TLS, unix sockets, IPv6, high load | `../src/http_client/` |
| `httpc_cookie_SUITE.erl` | cookie handling (`cookies` option, persistence, attributes) | `../src/http_client/` |
| `httpc_proxy_SUITE.erl` | proxy support (http/https, local/dummy/remote proxy groups) | `../src/http_client/` |
| `httpd_SUITE.erl` | `httpd` end-to-end: mod_get/head/cgi/esi/auth/alias/dir/log/disk_log, reload, limits, TLS variants | `../src/http_server/` |
| `httpd_basic_SUITE.erl` | protocol limits and slow-client handling (413/414, HTTP/0.9, script timeout, keep-alive) | `../src/http_server/` |
| `httpd_serve_SUITE.erl` | `httpd:serve/1`, `erl -S httpd serve`, bind addresses/IPv4+IPv6 | `../src/http_server/` |
| `httpd_bench_SUITE.erl` | benchmarks vs inets httpd / dummy server / nginx / wget (`inets_bench.spec`) | `../src/http_server/` |
| `http_format_SUITE.erl` | `http_chunk`, `http_request`, `http_util` parsing/formatting | `../src/http_lib/` |
| `uri_SUITE.erl` | `http_uri:encode/decode` | `../src/http_lib/` |
| `inets_SUITE.erl` | `inets` app/appup files, service start/stop API | `../src/inets_app/` |
| `inets_sup_SUITE.erl` | supervision tree and httpd/httpc subtree start-up | `../src/inets_app/` |
| `inets_socketwrap_SUITE.erl` | httpd on a pre-opened fd via setuid socket wrapper | `../src/http_server/` + `inets_socketwrap_SUITE_data/` |
| `inets_test_lib.erl` (+`.hrl`) | shared helpers: app start/stop, socket helpers, cert generation, free-port allocation | used by all suites |
| `http_test_lib.erl` | fake HTTP/HTTPS/unix-socket server + big-data listener | used by `httpc_SUITE` |
| `httpd_test_lib.erl`, `httpd_1_0.erl`, `httpd_1_1.erl` | server request verification and HTTP/1.0+1.1 request generators | used by `httpd_SUITE` |
| `httpd_load.erl`, `httpd_poll.erl`, `httpd_time_test.erl` | load/poll/timing helpers | used by `httpd_SUITE` |
| `httpd_bench_certs.erl` | cert material for benchmarks | used by `httpd_bench_SUITE` |
| `inets.spec`, `inets_bench.spec`, `inets.cover`, `inets.config`, `Makefile` | test harness entry points, coverage spec, module build list | this directory |
| `httpd_SUITE_data/`, `httpd_basic_SUITE_data/`, `httpd_serve_SUITE_data/`, `httpc_SUITE_data/`, `httpc_proxy_SUITE_data/` | per-suite CT data dirs (server roots, CGI sources, proxy scripts, payloads) | `../src/http_server/`, `../src/http_client/` |
| `inets_socketwrap_SUITE_data/` | setuid socket-wrapper C source (built at test time) | `../src/http_server/` |
| `httpd_test_data/`, `property_test/`, `inets_SUITE_data/` | legacy/dormant/placeholder dirs — not used by any suite | — |

## See Also
- `../src/http_server/`, `../src/http_client/`, `../src/http_lib/`, `../src/inets_app/` — the code under test.
- `../CONTEXT.md` — inets application overview and build layout.
- `../../common_test/test_server/` — `ts:compile_datadirs/2`, `ts_install_cth` and the `ts:run/3` harness referenced by these suites.
