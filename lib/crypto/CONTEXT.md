# crypto — OTP Application

## Intent
The `crypto` OTP application: a low-level Erlang API over the system OpenSSL/LibreSSL `libcrypto`, providing hashes, MACs, symmetric ciphers (incl. AEAD), public-key techniques, KDFs and the `rand`/RNG plugin machinery.
The Erlang code is a thin facade that validates arguments, chunks large data and delegates all cryptography to a single NIF shared library built from `c_src/`.
It is the cryptographic foundation that `public_key` and `ssl` build on.
App version lives in `vsn.mk` (`CRYPTO_VSN = 5.10`); the release-wide OTP→app version map is `../../otp_versions.table`.

## API Surface
- `src/crypto.erl` — the single public entry point (`-module(crypto)`); the whole reference manual is generated from its inline `-moduledoc`/`-doc` attributes.
  Main groups: `hash/2`, `hash_init/update/final`, `hash_xof`; `mac/init/update/final`, `cmac`, `hmac`, `poly1305`, `siphash`; the "ng" cipher API `crypto_init/update/final/one_time`, `crypto_one_time_aead`; `sign/verify`, `generate_key`, `compute_key`, `public_encrypt`/`private_decrypt`, KEM `encapsulate_key`/`decapsulate_key`; KDFs `pbkdf2_hmac/5`, `kdf/4` (argon2/hkdf/sskdf/scrypt); `strong_rand_bytes/1`, `strong_rand_range/1`, `rand_seed*`, `rand_plugin_*`; info/query `info/0`, `info_lib/0`, `info_fips/0`, `supports/0,1`, `enable_fips_mode/1`, `cipher_info/1`; and the `engine_*` OpenSSL ENGINE group.
- `src/crypto_ec_curves.erl` — internal EC curve name/number/alias tables (`curve/1`, `curves/0`, `curve_with_name/1`), consumed by `crypto:ec_curve(s)` and `public_key`; `-moduledoc false`.
- `src/crypto.app.src` — app resource: `{applications,[kernel,stdlib]}`, `{env,[{fips_mode,false},{rand_cache_size,896}]}`, `{vsn,"%VSN%"}`; `src/crypto.appup.src` is the release-upgrade script.
- NIF ABI: `crypto.erl` `-define(CRYPTO_NIF_VSN, 302)` is checked by the C side at load; the `.so`/`.dll` is loaded by `on_load/0` from `code:priv_dir(crypto)/lib/crypto`.
- `Makefile` — builds `src` `c_src` `doc` (`SUB_DIRECTORIES`); `static_lib` restricts to `c_src`; `VSN = $(CRYPTO_VSN)`.
- `configure` / `configure.ac` — per-app autoconf (prereq 2.72) run by the top-level `configure`; includes `otp.m4` and the shared OpenSSL detection `../../erts/crypto.ac`, then substitutes `c_src/Makefile.in` → `c_src/$host/Makefile`.

## Constraints
- `configure` is generated from `configure.ac`: edit the `.ac`/`.in` sources and run `../../otp_build update_configure`; never hand-edit `configure`.
- The Erlang NIF stubs / `-nifs([...])` list in `src/crypto.erl` and the `nif_funcs[]` table + atoms + arg arities in `c_src/` must stay in lockstep; `CRYPTO_NIF_VSN` (Erlang) and the C `vernum` check must be bumped together on any incompatible change.
- Algorithm availability is determined at build/run time by the linked OpenSSL version and by FIPS mode — never assume an algorithm exists; gate on `supports/1`/`info_fips/0` and the C-side `HAVE_*`/`FIPS_SUPPORT` macros.
- Code compiles with `-Werror` on both sides; the C code must support both old and new OpenSSL APIs (`HAS_3_0_API`, `evp_compat.h`, LibreSSL).
- App-level edits must not use absolute/worktree build paths.

## Dependencies
- System OpenSSL/LibreSSL headers + libcrypto, located by `configure` (`SSL_LIBDIR`, `SSL_INCLUDE`, `SSL_CRYPTO_LIBNAME`, …) via `../../erts/crypto.ac`, which is shared with the `ssl` app.
- Configure knobs reaching this app: `--enable-fips` (`-DFIPS_SUPPORT`), `CRYPTO_DISABLE_EVP_DH`/`CRYPTO_DISABLE_EVP_HMAC`, `DISABLE_OTP_TEST_ENGINE`, `CRYPTO_NO_DEPRECATE_WARN`.
- OTP app dependencies: `kernel`, `stdlib` only (`runtime_dependencies` erts-9.0 / stdlib-3.9 / kernel-6.0).
- FIPS mode needs a FIPS-capable OpenSSL; OpenSSL ENGINE usage is a deprecated API.

## Test Strategy
- Common Test suites in `test/` (`crypto_SUITE`, `engine_SUITE`, `crypto_property_test_SUITE`, plus benchmark suites); run via `make test` in this dir (or `make crypto_test` at the root), which releases the tests and runs `ct_run -spec crypto.spec`.
- Results depend on the linked libcrypto version and whether it can enter FIPS mode; most suites skip (rather than fail) unavailable algorithms/engines.
- The build (`make`) must succeed before tests run — tests exercise the real NIF/OpenSSL, not a stub.

## Notes for Agents
- `ebin/` and `priv/` are build outputs (git-ignored/untracked); `priv/lib/` holds the compiled NIF shared objects and is created by the build.
- There is no `include/` directory in this app — `crypto` exports no public `.hrl` (the `-I../include` in `src/Makefile` is a no-op leftover).
- Module/API reference docs are inline in `src/*.erl`; `doc/` holds only prose guides, the app page and release notes.

## Routing Table
| Path | Description |
| --- | --- |
| `src/` | Erlang API (`crypto.erl`, `crypto_ec_curves.erl`), `crypto.app.src`/`.appup.src`, NIF-VSN/on-load contract — see `src/CONTEXT.md` |
| `c_src/` | C NIF implementation over OpenSSL (`crypto.c`, `api_ng.c`, `pkey.c`, `fips.c`, `engine.c`, `openssl_config.h`, …) and the separate `crypto_callback`/`otp_test_engine` libs — see `c_src/CONTEXT.md` |
| `test/` | Common Test suites + NIST/RFC vector data + specs — see `test/CONTEXT.md`; `test/property_test/` holds the PropEr properties |
| `doc/` | ExDoc config (`docs.exs`), app page (`crypto_app.md`), `guides/`, release `notes.md` — see `doc/CONTEXT.md` |
| `../../erts/crypto.ac` | Shared OpenSSL detection / configure variables (outside this node — read-only) |

## See Also
- `../public_key/`, `../ssl/` — higher-level apps that consume this API (`ssl` shares `erts/crypto.ac`).
- `../../erts/` — runtime system; `../../make/` — build-system makefiles (`otp_subdir.mk`, `app_targets.mk`, `doc.mk`).
