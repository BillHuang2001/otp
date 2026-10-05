# crypto Application — Test Suite

## Intent
Common Test suites for the `crypto` application (OTP 30.0-rc0): functional, known-answer (NIST/RFC vector) and property coverage of the `crypto` API against the linked OpenSSL/libcrypto, plus separate benchmark suites.
Tests run against a *released copy* of this directory (`crypto_test/` in the test root), because the suites unzip vector archives into their data dir at runtime.

## API Surface
- `crypto_SUITE.erl` (6059 lines / ~342 KB) — the main suite: API/error behaviour, `all_ciphers`, hashes, MACs, AEAD, KDFs, key exchange, node/FIPS info, concurrency, plus vector-driven KATs and `doctests` (executes `crypto` doc examples through `ct_doctest`).
- `engine_SUITE.erl` (~1100 lines) — OpenSSL ENGINE API (`crypto:engine_load/list/register/unload/...`) including sign/verify and encrypt/decrypt with engine-stored keys.
- `crypto_property_test_SUITE.erl` — thin driver that pumps the PropEr properties in `property_test/` through `ct_property_test:quickcheck/2`.
- `crypto_bench_SUITE.erl` — throughput benchmarks (block/stream/chacha) calibrated against OpenSSL AES-CBC baselines; emits results as CT events (uses `ct_event.hrl`, `nowarn_deprecated_function` for the legacy block/stream API).
- `crypto_collect_labmachine_info_SUITE.erl` — lab-only helper that records the test machine's OpenSSL build/library info into a shared directory for benchmark comparison.
- `crypto.spec` — full/lab run: all suites in `../crypto_test` except `crypto_bench_SUITE`.
- `crypto_gh.spec` — GitHub Actions PR-gate variant of `crypto.spec`; additionally skips `engine_SUITE:multiple_engine_load`; auto-selected when `SPEC_POSTFIX=gh`.
- `crypto_bench.spec` — benchmark-only run: `crypto_collect_labmachine_info_SUITE` + `crypto_bench_SUITE`.
- `crypto.cover` — coverage spec (`{incl_app,crypto,details}`) passed to `ct_run`/`ts` with `-cover` when coverage is wanted.
- `crypto_SUITE_data/` — NIST/RFC KAT vector archives and pre-extracted vector files consumed by `crypto_SUITE`.
- `engine_SUITE_data/pkcs8/` — PEM RSA/DSA/ECDSA/EdDSA keys, plain and password-protected, used by `engine_SUITE` (the engine binaries themselves live in `../c_src`, reached via `crypto:get_test_engine/0` and `priv/libfail_engine.so`).
- `property_test/` — PropEr property modules (own CONTEXT.md).
- `Makefile` — `MODULES` lists the five suites; `release_tests_spec` installs suites + specs + cover into `$(RELEASE_PATH)/crypto_test` and tars `*_SUITE_data` and `property_test` there.

## Test Strategy
- Default run: `make test` in `lib/crypto` (or `make crypto_test` at the repo root) releases the tests and runs `ct_run -spec crypto.spec`; `ARGS` overrides the spec, e.g. `make test ARGS="-suite engine_SUITE -case ensure_load"`.
- `crypto_SUITE` is vector-driven: `init_per_suite` cds into the data dir and unzips `KAT_AES.zip` (NIST AESAVS KAT), `aesmmt.zip` (AES MMT), `cmactestvectors.zip` (NIST CMAC, ~8.5 MB) and `gcmtestvectors.zip` (NIST GCM, ~8.8 MB); the AES-CCM CAVS sets (`VADT*`/`VNT*`/`VPT*`/`DVPT*`) exist both pre-extracted and in `aes_ccm_vectors/ccmtestvectors.zip`.
- That vector volume makes `crypto_SUITE` by far the most expensive suite here (time and memory); expect a full crypto run to be dominated by it.
- Run a subset with `ct_run -spec <spec> -suite <SUITE> -case <case>` or via `make test ARGS="-suite ... -case ..."`; use `crypto_bench.spec` for benchmark-only runs and `crypto.cover` for coverage runs.
- Benchmarks are deliberately excluded from `crypto.spec` ("Benchmarks run separately") — never assume a normal test run exercises `crypto_bench_SUITE`.
- Skips are pervasive and environment-driven: `engine_SUITE` skips as a whole when engines are `notsup`/statically linked, on Darwin and Windows, and for old OpenSSL/LibreSSL; individual cases also skip when a cipher/hash/MAC is unavailable.
- FIPS coverage is the `fips` group inside `crypto_SUITE` (vs `non_fips`); it skips unless the linked cryptolib can actually enter FIPS mode (`crypto:enable_fips_mode/1`, provider-based OpenSSL 3).
- Property tests need PropEr; the stateful property is skipped when the available tool is QuickCheck/Triq (see `property_test/CONTEXT.md`).
- `crypto_collect_labmachine_info_SUITE` is lab infrastructure: it needs the `collect_host_info` CT config variable plus filesystem or `scp` access, and only makes sense in the OTP lab.

## Constraints
- New suites must be added to `MODULES` in `Makefile`; `*_SUITE_data` dirs and `property_test/` are shipped automatically by the `tar cf -` rule.
- `crypto_SUITE_data/` holds large third-party binary vector archives (NIST/RFC) — never hand-edit, regenerate or repack them.
- The test directory must be writable: the suites unzip archives into their data dir (the Makefile `chmod -R u+w` the released copy).
- Suites use `ts_install_cth` (from `lib/common_test/test_server`), so they are intended to run via the `ts`/`ct_run` wrapper, not by compiling the source dir with bare `ct_run`.
- Never assume an algorithm exists: gate on `crypto:supports/1`, `crypto:info_fips/0`, `crypto:info_lib()` and accept `error:notsup` / skip instead of failing.
- Extending the `crypto` API normally means updating the suite's export/support expectations in `crypto_SUITE.erl`, not just adding a test case.

## Known Issues
- `engine_SUITE:multiple_engine_load` is broken with OpenSSL 1.1.1f under docker, which is why `crypto_gh.spec` skips it.
- The unzipped multi-MB CMAC/GCM `.rsp` files (e.g. `CMACVerAES192.rsp` ~8 MB, `gcmEncryptExtIV256.rsp` ~3 MB) are written into the data dir on every run and then parsed line by line — the main reason a crypto test run is slow and disk hungry.

## Dependencies
- System OpenSSL/libcrypto: the linked version decides which cases run; provider-based features (FIPS, some KDFs) need OpenSSL 3.
- An OpenSSL build that supports dynamically loadable ENGINEs is required for `engine_SUITE` to do anything beyond skipping.
- PropEr (optional) for the stateful property test; `common_test` supplies `ct_property_test` and `ts_install_cth`.

## Notes for Agents
- `crypto_SUITE.erl` is intentionally one enormous module; do not try to split it, and do not document individual cases here — the case list lives in its `all/0` and `groups/0`.
- A new failing case is most often a missing `notsup`/version skip guard, not a real regression.
- Do not add test cases from this node's angle alone: checks that mirror doc examples belong in the `doctests` case, which fails when a documented example has no test.

## Routing Table
- `property_test/` → PropEr property modules for the new crypto API (`crypto_ng_api`, `crypto_ng_api_stateful`, generators)
- `crypto_SUITE_data/` → NIST/RFC KAT vector archives and extracted `.rsp`/`.txt` vectors (`aes_ccm_vectors/` subtree) — data only, no code
- `engine_SUITE_data/pkcs8/` → PEM keys for `engine_SUITE` — data only
- `../c_src/` → C sources, incl. the `otp_n`/`fail_engine` test engines loaded by `engine_SUITE` (sibling — read-only, escalate writes to parent)
- `../src/` → the `crypto` modules under test (sibling — read-only, escalate writes to parent)
