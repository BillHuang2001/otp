# ssl Application — Test Suite Directory

## Intent
Holds all Common Test suites and test helpers for the `ssl` application (TLS/DTLS implementation, SSL_VSN 11.7.7).
57 `*_SUITE.erl` suites cover the public `ssl` API, the TLS 1.2/1.3 and DTLS 1.2 record/handshake layers, certificates/CRLs/OCSP, sessions/tickets, distribution-over-TLS, and interop against the OpenSSL CLI.
Sources live in `../src/` and are handled elsewhere — this node only documents the test tree (read-only cross-reference).

## API Surface

### Shared helper libraries (compiled into `ebin/`, not suites)
- `ssl_test_lib.erl` (~4460 lines) — the central helper: `clean_start/0`, `init_per_suite/2` + `init_per_group/2` + `init_per_group_openssl/2`, `cert_options/1`, `ssl_options/2,3`, `start_server/start_client/start_upgrade_*/start_*_error`, `run_where/1`, `basic_test/3`, `test_ciphers/3`, `reuse_session/3`, `openssl_*` capability probes, `portable_cmd/2` + `portable_open_port/2`, `kill_openssl/0`, `trace/0`, `hardcode_rsa_key/1`, `ktls_*` helpers, `pss_params/1`, `gen_conf/4`.
Treat it as a god-module and extend it rather than adding parallel helpers; it is large but intentionally unsplit.
- `ssl_test_lib.hrl` — the CT macros suites should use: `?CT_LOG/1,2`, `?CT_PAL/1,2`, `?CT_FAIL/2` (they prefix pid/module/line/function).
- `ssl_cert_tests.erl` — shared test-case bodies and per-algorithm certificate config builders (`rsa_config/1`, `rsa_pss_config/2`, `dsa_config/1`, `ecdsa_config/1`, `eddsa_config/1`, `mldsa_config/1`, `slh_dsa_sha2/shake_config/1`, `support_kems/1`) reused by `ssl_cert_SUITE` and the two `openssl_*_cert_SUITE`s.
- `make_certs.erl` — builds a full OpenSSL CA hierarchy in the test `priv_dir` by driving the OpenSSL CLI (`openssl req/ca/ecparam/crl`): `all/2,3`, `rootCA/3`, `intermediateCA/4`, `enduser/4`, `revoke/3`, `gencrl/4`, `make_config/1`.
- `x509_test.erl` — turns `public_key:pkix_test_data/1` output into PEM cert/key/cacert files (`gen_pem_config_files/3`) plus `extensions/1`.
- `ssl_ECC.erl` — ECC handshake test bodies (`client_ecdhe_ecdsa_server_ecdh_rsa/1`, …) used by `ssl_ECC_SUITE`/`openssl_ECC_SUITE`.
- `ssl_dist_test_lib.erl` + `ssl_dist_test_lib.hrl` — helpers for bringing up `-proto_dist inet_tls` nodes; the header defines `#node_handle{}`.
- `ssl_bench_test_lib.erl` — benchmark peer setup; honours env var `NETMARKS_REMOTE_HOST` (spawns the peer over `ssh` when set).
- `cryptcookie.erl` + `dist_cryptcookie.erl` — prototype distribution backends (own cookie/key-pair handshake, plus kTLS info export) used only by the dist benchmarks.
- `inet_epmd_cryptcookie_inet_ktls.erl`, `inet_epmd_cryptcookie_socket_ktls.erl`, `inet_epmd_dist_cryptcookie_inet.erl`, `inet_epmd_dist_cryptcookie_socket.erl` — `-inet_epmd` backend modules selected via `-proto_dist inet_epmd -inet_epmd <name>`; each exposes `supported/0` for group-level skipping.

### Suite families
- Core public API / protocol: `ssl_api_SUITE.erl` (largest, ~223 KB: option validation, `certs_keys`, handshake/3 options, transport abuse, keylog), `tls_api_SUITE.erl`, `dtls_api_SUITE.erl`, `ssl_basic_SUITE.erl`, `ssl_alert_SUITE.erl`.
- Record/handshake layer: `ssl_handshake_SUITE.erl`, `ssl_socket_SUITE.erl`, `ssl_packet_SUITE.erl` (packet framing), `ssl_payload_SUITE.erl` (payload/record sizes), `tls_1_3_record_SUITE.erl`, `tls_1_3_version_SUITE.erl`, `ssl_rfc_5869_SUITE.erl` (HKDF test vectors), `ssl_reject_SUITE.erl`.
- Certificates: `ssl_cert_SUITE.erl`, `openssl_server_cert_SUITE.erl`, `openssl_client_cert_SUITE.erl` (all driven by `ssl_cert_tests.erl`).
- Ciphers / curves: `ssl_cipher_SUITE.erl`, `ssl_cipher_suite_SUITE.erl`, `openssl_cipher_suite_SUITE.erl`, `ssl_ECC_SUITE.erl`, `openssl_ECC_SUITE.erl`.
- CRL / OCSP: `ssl_crl_SUITE.erl` (generates CRLs with `make_certs`, serves them over an `inets`/`httpd` instance), `ssl_crl_white_box_SUITE.erl`, `openssl_stapling_SUITE.erl` (OCSP stapling, starts an OpenSSL OCSP responder).
- Sessions / tickets / keys: `ssl_session_SUITE.erl`, `ssl_session_cache_SUITE.erl`, `ssl_session_cache_api_SUITE.erl`, `ssl_session_ticket_SUITE.erl`, `tls_server_session_ticket_SUITE.erl`, `tls_client_ticket_store_SUITE.erl`, `openssl_session_SUITE.erl`, `openssl_session_ticket_SUITE.erl`, `ssl_key_update_SUITE.erl`, `openssl_key_update_SUITE.erl`, `ssl_renegotiate_SUITE.erl`, `openssl_renegotiate_SUITE.erl`, `ssl_upgrade_SUITE.erl` (appup/relup via `ct_release_test`).
- Feature suites: ALPN (`ssl_alpn_SUITE`, `openssl_alpn_SUITE`), SNI (`ssl_sni_SUITE`, `openssl_sni_SUITE`), NPN (`ssl_npn_SUITE`, `ssl_npn_hello_SUITE`, `openssl_npn_SUITE`), max fragment length (`ssl_mfl_SUITE`, `openssl_mfl_SUITE`), SRTP (`ssl_use_srtp_SUITE`).
- Distribution: `ssl_dist_SUITE.erl` (multi-node `peer`-started distribution over TLS, incl. kTLS cases), `ssl_dist_bench_SUITE.erl` (dist backends incl. cryptcookie/kTLS).
- Property-based: `ssl_eqc_SUITE.erl` driving the modules in `property_test/` through `ct_property_test`.
- White-box / diagnostics: `ssl_trace_SUITE.erl` (`ssl_trace` API), `ssl_pem_cache_SUITE.erl`, `ssl_app_env_SUITE.erl`, `ssl_engine_SUITE.erl` (OpenSSL engine, skipped when unavailable).
- Benchmarks: `ssl_bench_SUITE.erl` + `ssl_bench_test_lib.erl`, `ssl_dist_bench_SUITE.erl` (each has a `benchmark` group; `?COUNT`/`?REDUCE` scale the workload).

### Spec / coverage files
- `ssl.spec` — default spec used by `make test`: `{merge_tests,false}` + all suites in `../ssl_test` (the installed test release dir).
- `ssl_gh.spec` — spec used by GitHub CI (selected via `SPEC_POSTFIX=gh` in `../../../.github/workflows/reusable-test.yaml` and `reusable-system-test.yaml`): skips `ssl_upgrade_SUITE`, all benchmark groups and one unstable case, adds `cte_track` and `cth_log_redirect`.
- `ssl_bench.spec` — runs only the `benchmark` groups of `ssl_bench_SUITE`/`ssl_dist_bench_SUITE` (`ct_run -spec ssl_bench.spec`).
- `ssl.cover` — coverage config (`{incl_app,ssl,details}`, excludes `ssl_debug`), fed to `ct_run -cover`.

## Constraints & Dependencies
- `../Makefile` sets `TEST_NEEDS_RELEASE=true`: the suites always run against an *installed release*, so ssl source changes must be built and released before testing.
- External OpenSSL CLI on `$PATH` is required by every suite doing real interop and by `make_certs.erl`; the version is probed once via `ssl_test_lib:portable_cmd("openssl", ["version"])` and cached in the process dictionary (`put(openssl_version, …)`).
- OpenSSL version gates live in `ssl_test_lib` (`check_sane_openssl_version/2`, `openssl_sane_dtls/0`, `openssl_maxfraglen_support/0`, `openssl_ocsp_support/1`, `enough_openssl_crl_support/1`, `check_openssl_npn_support/1`, `is_sane_openssl_pss/1`, `working_openssl_client/1`) — these exist because the `openssl_*` suites must skip on broken/old OpenSSL builds (LibreSSL is tolerated). `ssl_test_lib:init_per_suite/2` throws `{skip, "Unknown openssl version: …"}` for anything not starting with `OpenSSL`/`LibreSSL`.
- On Windows the suites run the *Linux* `openssl` through `wsl` when `WSLENV` is set (`portable_open_port/2`, `kill_openssl/0` uses `wsl pkill` vs `taskkill openssl.exe`).
- `ssl_engine_SUITE` additionally needs a crypto/OpenSSL test engine (`crypto:get_test_engine()`, dynamic engine support) and skips otherwise; kTLS cases need `inet_tls_dist:ktls_os()` support (Linux/FreeBSD) and are skipped elsewhere.
- Certificates come from two independent mechanisms: (a) the OpenSSL-CLI CA generator `make_certs.erl` writing under `priv_dir` (`erlangCA`/`otpCA` roots, `client`/`server`/`a.server`/`b.server`/`localhost`/`revoked`/`undetermined` end entities — topology documented in `../internal_doc/ssl_notes.md`), and (b) in-Erlang generation via `ssl_test_lib:make_*_cert/1` + `public_key:pkix_test_data/1` + `x509_test:gen_pem_config_files/3` (deterministic keys from `ssl_test_lib:hardcode_rsa_key/1`).
- `ssl_cert_tests:mldsa_config/1` reads pre-generated ML-DSA (post-quantum) keys from the suite `data_dir` — that is what `*_SUITE_data/mldsa-{44,65,87}[-pub].pem` are for.
- Networking: suites default to client and server on the *same* node (`ssl_test_lib:run_where/1`); `{node, N}` options and `peer:start/1` are used by the distribution and some CRL tests, which need epmd and unique node names.
- Test ports are ephemeral (`{port, 0}` + `ssl_test_lib:inet_port/1`); `ssl_crl_SUITE` additionally needs a free local HTTP port for its CRL distribution point.
- `Makefile` `DATADIRS = ssl_basic_SUITE_data` is the only data dir declared as such; `release_tests_spec` copies every `*_SUITE_data/` plus `property_test/` into the installed `ssl_test` release directory.

## Test Strategy
- Run a single suite interactively: `make ARGS="-suite ssl_cipher_SUITE" test` from `lib/ssl/` (ARGS is forwarded to `ct_run`; `-case <case>` also works).
- Run everything: `make test` from `lib/ssl/` — `make/test_target_script.sh` releases Erlang to `lib/ssl/make_test_dir`, installs the suites as `ssl_test`, compiles `*_SUITE_data` (`ts:compile_datadirs`), then `ct_run -spec …/ssl_test/ssl.spec`; logs land in `lib/ssl/make_test_dir/ct_logs`.
- Benchmarks: `ct_run -spec ssl_bench.spec` (or `-suite ssl_bench_SUITE -group benchmark`); export `NETMARKS_REMOTE_HOST` to run them against a second host over ssh.
- CI uses `SPEC_POSTFIX=gh` to select `ssl_gh.spec`; expect the GH skip list (benchmarks, `ssl_upgrade_SUITE`) to differ from a local `make test` run.
- Expect the full suite set to take hours (it is multi-node, network-bound and runs OpenSSL peers); use `-suite`/`-group` filters during development.
- No `*_SUITE_data` fixtures are checked in beyond certs/DH params — most material (certificates, CRLs, PEMs) is generated into `priv_dir` per run.

## Known Issues / Notes for Agents
- `ssl_test_lib:cert_options/1` expects PEMs under `priv_dir` (`client/cert.pem`, `server/keycert.pem`, `dsa*`, `ecdsa*`, `rsa*`, …); a suite that forgets to call a `make_*_cert/1` in `init_per_suite` fails with confusing `enoent` errors.
- `make_certs:create_rnd/2` copies `RAND` from the suite `data_dir` into the generated CA tree (OpenSSL `RANDFILE`); the copy's error is ignored, so a missing `RAND` silently degrades to a freshly seeded openssl RNG — only `ssl_basic_SUITE_data/RAND` exists.
- `ssl.cover` still excludes the `ssl_debug` module, which no longer exists (only the `-Dssl_debug` compile flag in `../src/ssl_alert.hrl`) — harmless but misleading.
- `ssl_dist_SUITE_data/dHParam.pem` is not referenced by any current suite or helper code (leftover fixture).
- `ssl_upgrade_SUITE` depends on `ct_release_test` and the appup files being available; it is skipped in the GitHub CI spec as "Versions not always correct".
- Suites rely on the process dictionary for `openssl_version` (set in `init_per_suite/2` / `init_per_group*`) — do not run suite cases without the suite/group init having run.
- Editing any suite requires the test modules to be re-released (`TEST_NEEDS_RELEASE=true`) before `ct_run` picks them up.

## Routing Table
| Path | Contents |
| --- | --- |
| `ssl_test_lib.erl`, `ssl_test_lib.hrl` | Core test helper + CT logging macros (reuse these, do not reinvent) |
| `ssl_cert_tests.erl`, `x509_test.erl`, `make_certs.erl`, `ssl_ECC.erl` | Certificate/key material generation and shared test-case bodies |
| `ssl_api_SUITE.erl`, `tls_api_SUITE.erl`, `dtls_api_SUITE.erl`, `ssl_basic_SUITE.erl`, `ssl_alert_SUITE.erl` | Public API and basic protocol suites |
| `ssl_handshake_SUITE.erl`, `ssl_socket_SUITE.erl`, `ssl_packet_SUITE.erl`, `ssl_payload_SUITE.erl`, `tls_1_3_*_SUITE.erl`, `ssl_rfc_5869_SUITE.erl`, `ssl_reject_SUITE.erl` | Record/handshake layer and framing |
| `ssl_cert_SUITE.erl`, `openssl_{server,client}_cert_SUITE.erl`, `ssl_cipher*_SUITE.erl`, `openssl_cipher_suite_SUITE.erl`, `ssl_ECC_SUITE.erl`, `openssl_ECC_SUITE.erl` | Certificate, cipher and curve matrices |
| `ssl_crl_SUITE.erl`, `ssl_crl_white_box_SUITE.erl`, `openssl_stapling_SUITE.erl` | CRL and OCSP(-stapling) |
| `ssl_session*_SUITE.erl`, `tls_{client,server}_*_SUITE.erl`, `openssl_session(_ticket)_SUITE.erl`, `ssl_key_update_SUITE.erl`, `openssl_key_update_SUITE.erl`, `ssl_*renegotiate_SUITE.erl`, `ssl_upgrade_SUITE.erl` | Sessions, tickets, key update, renegotiation, appup upgrade |
| `ssl_alpn_SUITE.erl`, `openssl_alpn_SUITE.erl`, `ssl_sni_SUITE.erl`, `openssl_sni_SUITE.erl`, `ssl_npn*_SUITE.erl`, `openssl_npn_SUITE.erl`, `ssl_mfl_SUITE.erl`, `openssl_mfl_SUITE.erl`, `ssl_use_srtp_SUITE.erl` | ALPN / SNI / NPN / MFL / SRTP features |
| `ssl_dist_SUITE.erl`, `ssl_dist_bench_SUITE.erl`, `ssl_dist_test_lib.erl`, `ssl_dist_test_lib.hrl`, `cryptcookie.erl`, `dist_cryptcookie.erl`, `inet_epmd_*.erl` | Distribution-over-TLS tests, backends and benchmarks |
| `ssl_eqc_SUITE.erl` | Property-test driver (see `property_test/`) |
| `property_test/` | EQC/QuickCheck property modules (`ssl_eqc_handshake`, `ssl_eqc_chain`, `ssl_eqc_cipher_format`, `ssl_eqc_ecc`) — own CONTEXT.md |
| `ssl_trace_SUITE.erl`, `ssl_pem_cache_SUITE.erl`, `ssl_app_env_SUITE.erl`, `ssl_engine_SUITE.erl` | White-box / diagnostic / OpenSSL-engine suites |
| `ssl_bench_SUITE.erl`, `ssl_bench_test_lib.erl` | TLS throughput/latency benchmarks (`benchmark` group) |
| `ssl.spec`, `ssl_gh.spec`, `ssl_bench.spec`, `ssl.cover` | ct_run specs (default / GitHub CI / benchmark) and coverage config |
| `*_SUITE_data/` | Static fixtures: ML-DSA keys (`ssl_cert_SUITE_data`, `openssl_{client,server}_cert_SUITE_data`), DH params/cert/key (`ssl_api_SUITE_data`), RAND seed (`ssl_basic_SUITE_data`), unused `dHParam.pem` (`ssl_dist_SUITE_data`) |
| `Makefile` | Compiles all helpers/suites into `ebin/`, defines `MODULES`/`DATADIRS`, installs suites + specs + cover file into `$(RELEASE_PATH)/ssl_test` |
| `../src/` | ssl implementation under test (read-only cross-reference) |
| `../internal_doc/ssl_notes.md` | Test certificate topology and OCSP-stapling design notes (read-only cross-reference) |

## See Also
- `../CONTEXT.md` — the ssl application node; `../src/CONTEXT.md` — implementation modules under test.
- `../../../make/test_target_script.sh` — the harness behind `make test` (`TEST_NEEDS_RELEASE`, `SPEC_POSTFIX`, `ARGS`).
- `../../../HOWTO/TESTING.md` — general Erlang/OTP test instructions.
