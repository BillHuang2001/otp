# crypto NIF — C sources

## Intent
Implements the C NIF library of the `crypto` application: `crypto.so`/`.dll` (loaded by `../src/crypto.erl`), plus the companion `crypto_callback` and `otp_test_engine` libraries.
All cryptographic primitives are delegated to the system OpenSSL (or LibreSSL) `libcrypto`; this directory contains only glue, argument/term conversion and version-abstraction code.
Leaf node: every file is a translation unit of one concern, no subdirectories (the per-host build dirs are generated and git-ignored).

## API Surface
### Build
- `Makefile` — thin wrapper that includes `$(ERL_TOP)/make/run_make.mk`; all targets recurse into `$(TARGET)/Makefile`.
- `Makefile.in` — the real build description; `../configure.ac` substitutes it into `c_src/$host/Makefile` (`AC_CONFIG_FILES([c_src/$host/Makefile:c_src/Makefile.in])`).
- Object lists: `CRYPTO_OBJS` (one `.o` per `.c`, list is authoritative), `CALLBACK_OBJS` (`crypto_callback.o`), `TEST_ENGINE_OBJS` (`otp_test_engine.o`), and `_static` variants for `crypto.a` (`static_lib` target).
- Outputs go to `../priv/obj/$(TARGET)` and `../priv/lib/$(TARGET)`; `TYPE=debug|valgrind|asan|gprof|(opt)` adds a suffix marker and matching flags.
- `HAVE_DYNAMIC_CRYPTO_LIB` (from `@SSL_DYNAMIC_ONLY@` or win32) decides whether `crypto_callback` is a separate lib and whether `crypto_callback.o` is linked into `crypto`.
### NIF plumbing
- `crypto.c` — `ERL_NIF_INIT(crypto,...)` entry: `nif_funcs[]` table (name, arity, C function, flags), `load`/`upgrade`/`unload`, OpenSSL version check, OSSL provider loading, atom/type-table init, `crypto_callback` dlopen.
- `api_ng.c`/`api_ng.h` — the unified cipher/AEAD API (`ng_crypto_init/update/final/one_time/get_data_nif`).
- `atoms.c`/`atoms.h` — pre-created atom table (`init_atoms`); many atoms are `#ifdef`-gated by feature macros.
- `algorithms.c`/`algorithms.h` — algorithm enumeration exposed to Erlang (`hash_algorithms`, `pubkey_algorithms`, `cipher_algorithms`, `mac_algorithms`, `curve_algorithms`, `rsa_opts_algorithms`, `kem_algorithms_nif`), FIPS-filtered variants, and the curve mutex.
- `common.c`/`common.h` — `raise_exception` plus the `EXCP_*`/`RAISE_NOTSUP`/`assign_goto` macros used by every module.
- `info.c`/`info.h` — `info_nif` (compile type, dynamic/static link type, cryptolib compiled vs linked version, FIPS provider info), `info_lib`, `resource_name` (version-stamped resource type names), `change_basename` + `error_handler` for loading the callback lib.
- `crypto_callback.c`/`crypto_callback.h` — separate lib exporting `get_crypto_callbacks`; lets `libcrypto` allocate/free through `enif_alloc`/`enif_free` and (pre-1.1.0) use VM locking.
### Hashes and MACs
- `hash.c`/`hash.h`, `digest.c`/`digest.h` — hash context and digest type tables, legacy `hash_init/update/final` API, XOF output via `hash_final_xof_nif`.
- `mac.c`/`mac.h` — modern unified MAC API (`mac_nif`, `mac_init/update/final_nif`) covering HMAC/CMAC/Poly1305/SipHash.
- `hmac.c`/`hmac.h`, `cmac.c`/`cmac.h` — low-level helpers (`hmac_low_level`, `cmac_low_level`) used by `mac.c` and legacy paths.
- `hash_equals.c`/`hash_equals.h` — constant-time comparison.
### Symmetric ciphers
- `cipher.c`/`cipher.h` — legacy cipher table and `cipher_info_nif`.
- `aead.c`/`aead.h` — AEAD one-shot and streaming (`aead_cipher_init_nif`, `aead_cipher_nif` in 4- and 7-arg forms).
- `aes.c`/`aes.h` — AES cases not handled by EVP (AES-CTR compat path, AES-GCM without EVP).
- `evp.h`/`evp.c` — EVP-level helpers: `evp_compute_key_nif`, `evp_generate_key_nif`, KEM `encapsulate_key_nif`/`decapsulate_key_nif`.
### Asymmetric / pkey
- `pkey.c`/`pkey.h` — generic `pkey_sign_nif`, `pkey_sign_heavy_nif` (CPU-bound dirty NIF), `pkey_verify_nif`, `pkey_crypt_nif`, `privkey_to_pubkey_nif`, `prefetched_sign_algo_init` (OpenSSL >= 3.4); largest file, with an OSSL-param branch and a legacy branch.
- `rsa.c`, `dss.c`, `ec.c`, `ecdh.c`, `eddsa.c`, `dh.c` — per-algorithm key construction/generation/computation over EVP.
- `bn.c`/`bn.h` — BIGNUM ↔ Erlang integer/binary (`mpint`) conversion, `mod_exp_nif`, OSSL_PARAM helpers.
- `srp.c`/`srp.h` — SRP value and secret computation.
- `kdf.c`/`kdf.h` — KDF API (`kdf_nif`, `kdf_algorithms`), compiled only under `HAVE_KDF` (OpenSSL 3.0+).
- `pbkdf2_hmac.c`/`.h` — PBKDF2-HMAC.
### Random and misc
- `rand.c`/`rand.h` — `strong_rand_bytes_nif`, `strong_rand_range_nif`, `rand_uniform_nif`, `rand_seed_nif`.
- `math.c`/`math.h` — `do_exor`.
### FIPS and ENGINE
- `fips.c`/`fips.h` — `info_fips`, `enable_fips_mode_nif`, and `enable_fips_mode` called at module load; compiles to no-ops when `FIPS_SUPPORT` is undefined.
- `engine.c`/`engine.h` — OpenSSL ENGINE binding (deprecated OpenSSL API) gated by `HAS_ENGINE_SUPPORT`; also engine mutex and `get_engine_and_key_id`/`get_key_password` used by `pkey.c`.
- `otp_test_engine.c` — self-contained OpenSSL ENGINE with id/name `MD5`, built as a separate lib unless `DISABLE_OTP_TEST_ENGINE=yes`; used only by the test suite.
### Version detection / configuration
- `openssl_version.h` — `PACKED_OPENSSL_VERSION[_PLAIN]` helpers and `HAS_LIBRESSL`/`HAS_LIBRESSL_VSN`.
- `openssl_config.h` — central feature-macro header: pulls in all OpenSSL headers and derives `HAS_3_0_API`, `HAS_LIBRESSL`, `NEED_EVP_COMPATIBILITY_FUNCTIONS`, `HAS_EVP_PKEY_CTX`, `HAS_CRYPTO_MEM_FUNCTIONS`, `HAS_ENGINE_SUPPORT`, `FIPS_SUPPORT`/`FIPS_MODE`, `DISABLE_EVP_DH`/`DISABLE_EVP_HMAC`, per-algorithm `HAVE_*` (SHA2/SHA3/SM3/SM4/BLAKE2/EC/EDDSA/EDDH/ARGON2/ML_KEM/ML_DSA/SLH_DSA/…), plus `MAX_BYTES_TO_NIF`, `CONSUME_REDS`, `PRINTF_ERR*`.
- `evp_compat.h` — shims replacing OpenSSL 1.1.0+ opaque-struct APIs (HMAC_CTX, RSA/DSA/DH set0/get0, BN_GENCB) with the pre-1.1.0 struct accessors; included only when `NEED_EVP_COMPATIBILITY_FUNCTIONS`.
### Static analysis
- `check_openssl.cocci` / `check_erlang.cocci` — Coccinelle semantic-patch rules verifying the subset of OpenSSL calls used and resource cleanup on all paths (`spatch --all-includes -sp_file <file> -dir .`).

## Constraints
- `nif_funcs[]` in `crypto.c` must match `-nifs([...])` in `../src/crypto.erl` exactly in name and arity; a name may be registered twice with different arities (e.g. `aead_cipher_nif`/4 and /7).
- The `load_info` tuple is `{?CRYPTO_NIF_VSN, LibBinPath, FipsMode}`; C hard-codes the accepted `vernum == 302`, so bump it together with `?CRYPTO_NIF_VSN` in `../src/crypto.erl` on any incompatible change.
- Every NIF must return a valid term or raise via `raise_exception`/`EXCP_*`; resource types must be named with `resource_name/2` and released on every path.
- Do not assume algorithm availability — gate on the `HAVE_*`/`HAS_*` macros, and on `FIPS_MODE()` where a FIPS-restricted set applies.
- Support both old and new OpenSSL APIs: new code must compile with and without `HAS_3_0_API`, `NEED_EVP_COMPATIBILITY_FUNCTIONS` and `HAS_LIBRESSL`.
- Deprecated OpenSSL ENGINE calls compile with warning suppression only in `engine.c`, `pkey.c` and `otp_test_engine.c` (via `$(CRYPTO_NO_DEPRECATE_WARN)`); using them elsewhere breaks `-Werror` builds.
- Long-running NIFs must use `CONSUME_REDS`/`enif_consume_timeslice`; genuinely heavy work must be marked dirty (`ERL_NIF_DIRTY_JOB_CPU_BOUND`, as in `pkey_sign_heavy_nif`).
- Code must stay `-Werror`-clean (incompatible pointer types, sign/size mismatches are actively fixed).
- Edit `Makefile.in`, not the generated `c_src/<host>/Makefile`; edit `../../erts/crypto.ac` for configure variables.

## Design Decisions
- One NIF shared object for the whole application; only `crypto_callback` is split out, so a shared/static `libcrypto` can call back into the VM (allocation, old locking) without a circular dependency.
- Version differences are resolved at compile time in `openssl_config.h` so the algorithm modules stay free of `#if` on OpenSSL versions.
- OpenSSL 3.x provider-based FIPS is emulated on the legacy API through `FIPS_mode()`/`FIPS_mode_set()` macros, and `crypto.c` loads the `default`/`base`/`legacy` providers at init (plus `fips` when built with `FIPS_SUPPORT`).
- `MAX_BYTES_TO_NIF` in `openssl_config.h` must stay in sync with the equivalent constant in `../src/crypto.erl` (they define the reduction accounting for a NIF byte count).
- `otp_test_engine` is a separate library (default) so the test ENGINE can be loaded/unloaded dynamically without touching the NIF.

## Known Issues / Gotchas
- `unload` intentionally performs no OpenSSL cleanup (musl `dlclose` is a no-op, macOS can pin the lib after `OSSL_PROVIDER_load`), relying on OpenSSL's own atexit handler; `unload_thread` (`ERL_NIF_OPT_ON_UNLOAD_THREAD`) calls `OPENSSL_thread_stop()` only for OpenSSL >= 1.1.0, non-LibreSSL.
- `library_refc`/`library_initialized` make a re-load during module upgrade a no-op after the first init: changed atoms, providers or type tables only take effect after a full VM restart.
- `verify_lib_version()` compares only the major version of linked lib vs compile-time header, and its message is compiled out unless `CRYPTO_DEVELOP_ERRORS` is set (all `PRINTF_ERR*` are no-ops by default).
- LibreSSL reports an `OPENSSL_VERSION_NUMBER` >= 2.0.0 while implementing the 1.0.1 API, so `HAS_LIBRESSL` must be tested before any version comparison.
- The `struct crypto_callbacks` layout plus the `ccb->sizeof_me != sizeof(*ccb)` check is a hard ABI between `crypto.so` and `crypto_callback.so`.
- The `crypto_callback` basename is type-dependent (`crypto_callback.debug|.valgrind|.asan`), computed from `DEBUG`/`VALGRIND`/`ADDRESS_SANITIZER` in `info.c`.

## Dependencies
- System OpenSSL/LibreSSL headers and library, located by configure: `SSL_LIBDIR`, `SSL_INCLUDE`, `SSL_CRYPTO_LIBNAME`, `SSL_SSL_LIBNAME`, `SSL_EXTRA_LIBS`, `SSL_DYNAMIC_ONLY`, `SSL_FLAGS`, `SSL_DED_LD_RUNTIME_LIBRARY_PATH` (all from `../../erts/crypto.ac`, shared with `ssl`).
- Configure knobs reaching this dir: `--enable-fips` (appends `-DFIPS_SUPPORT` to `SSL_FLAGS`), `CRYPTO_DISABLE_EVP_DH`, `CRYPTO_DISABLE_EVP_HMAC` (passed as `-D` via `CONFIGURE_ARGS`), `DISABLE_OTP_TEST_ENGINE`, `CRYPTO_NO_DEPRECATE_WARN`, `DED_*` toolchain flags, `HAVE_DYNAMIC_CRYPTO_LIB`.
- Available algorithms are determined by the OpenSSL headers/library at build and runtime, not by this directory.
- Coccinelle (`spatch`) is needed only to run the `.cocci` checks.
- Windows build dirs (`c_src/win32/`) and build outputs (`../priv/lib/`, `../priv/obj/`) are git-ignored (root `.gitignore`).

## Test Strategy
- No Erlang or C test suite lives here; behaviour is covered from `../test/` (`crypto_SUITE`, `crypto_bench_SUITE`, `crypto_property_test_SUITE`).
- `engine_SUITE` exercises `engine.c` and `otp_test_engine.c` by loading the `MD5` test engine (`crypto:get_test_engine()`).
- Rebuild variants with `TYPE=debug|valgrind|asan` and run the suites against that library; the `asan` variant links with `-fsanitize=address`.
- The `.cocci` files are the static checks for OpenSSL-call subset and resource cleanup.

## Notes for Agents
- `../src/crypto.erl` is the source of truth for NIF names, arities and `CRYPTO_NIF_VSN`; grep there when in doubt about a NIF's contract.
- Feature/algorithm availability is decided by the OpenSSL headers at build time — check the relevant `HAVE_*` macro in `openssl_config.h` before editing a module.
- This is a leaf node: all work belongs in the files here; sibling `../test/`, `../src/`, `../doc/` are separate nodes.
- Never edit generated files or build outputs; `c_src/<host>/Makefile` is produced by configure.
- The `../priv/` directory does not exist in a clean checkout; it is created by the build.

## Routing Table
- No child directories — this node holds only the `*.c`/`*.h` translation units listed above and the `.cocci` lint rules.
- `../src/` → Erlang API, `.app.src`, `CRYPTO_NIF_VSN` (sibling — read-only, escalate writes to parent).
- `../test/` → Common Test suites incl. `engine_SUITE` (sibling — read-only, escalate writes to parent).
- `../doc/` → application documentation (sibling — read-only, escalate writes to parent).
- `../../erts/crypto.ac` → shared OpenSSL detection/configure variables (outside this node — read-only).
