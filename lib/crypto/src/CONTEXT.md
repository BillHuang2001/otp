# crypto — Erlang Sources

## Intent
The Erlang side of the OTP `crypto` application: the public `crypto` API/facade that validates arguments, chunks data, and delegates algorithm work to a NIF built from `../c_src/`.
Contains exactly two modules plus build/release metadata — all actual cryptography lives in the C NIF.

## API Surface
- `crypto.erl` (~5285 lines) — `-module(crypto)`, the single public entry point; the application's whole documented API lives here in `-moduledoc`/`-doc`/`-doc(#{group => ...})` attributes (the reference manual is generated from these attributes, not from a `doc/src` dir).
  - Version/query: `info/0`, `info_lib/0`, `info_fips/0`, `supports/0,1`, `enable_fips_mode/1`, `cipher_info/1`, `hash_info/1`, `version/0`, `bytes_to_integer/1`.
  - Hashing: `hash/2`, `hash_xof/3`, `hash_init/1`, `hash_update/2`, `hash_final/1`, `hash_final_xof/2`, `hash_equals/2`.
  - MACs: `mac/3,4`, `macN/4,5`, `mac_init/2,3`, `mac_update/2`, `mac_final/1`, `mac_finalN/2`.
  - New cipher API ("ng"): `crypto_init/3,4`, `crypto_update/2`, `crypto_final/1`, `crypto_get_data/1`, `crypto_one_time/4,5`, `crypto_one_time_aead/4,6,7`, `crypto_one_time_aead_init/4`.
  - Asymmetric: `sign/4,5`, `verify/5,6`, `generate_key/2,3`, `compute_key/4`, `encapsulate_key/2`, `decapsulate_key/3`, `public_encrypt/4`/`private_decrypt/4`/`private_encrypt/4`/`public_decrypt/4`, `privkey_to_pubkey/2`, `mod_pow/3`.
  - KDFs: `pbkdf2_hmac/5`, `kdf/4` (argon2/sskdf/hkdf/pbkdf2/scrypt).
  - RNG: `strong_rand_bytes/1`, `strong_rand_range/1`, `exor/2`, `rand_seed/0,1`, `rand_seed_alg/1,2`, `rand_seed_s/0`, `rand_seed_alg_s/1,2`, `rand_plugin_*`, `rand_cache_plugin_*`, `rand_plugin_aes_*`, `rand_plugin_prng_*`, `rand_uniform/2`.
  - Curves/errors/engine: `ec_curve/1`, `ec_curves/0`, `format_error/2`, and the `engine_*` / `ensure_engine_loaded/2,3` / `ensure_engine_unloaded/1,2` group.
  - Private/test-only exports: `packed_openssl_version/4`, `engine_methods_convert_to_bitmask/2`, `get_test_engine/0`, `rand_plugin_aes_jump_2pow20/1`.
- `crypto_ec_curves.erl` (~1200 lines) — `-moduledoc false`, exports `curve/1`, `curves/0`, `curve_with_name/1`; static tables mapping EC curve name/number/alias to parameters (`prime_field` / `characteristic_two_field`).
  - Pure data module — no NIF use; `curves/0` derives its list from `crypto:supports()` minus `x25519`/`x448`.
  - `secp192r1`/`secp256r1` are aliased to `prime192v1`/`prime256v1` via `real_name/1`; consumed by `crypto:ec_curve(s)` and `public_key`.
- `crypto.app.src` (`{vsn,"%VSN%"}`, modules `[crypto, crypto_ec_curves]`, `{applications,[kernel,stdlib]}`, `{env,[{fips_mode,false},{rand_cache_size,896}]}`) and `crypto.appup.src` (`%VSN%` substituted at build; all upgrades do `restart_application, crypto`).
- `Makefile` — `MODULES = crypto crypto_ec_curves`; compiles with `-DCRYPTO_VSN=\"$(VSN)\" -Werror -I../include`; `VSN = $(CRYPTO_VSN)` from `../vsn.mk`; generates `ebin/crypto.app` and `ebin/crypto.appup` from the `.src` files via `sed 's;%VSN%;$(VSN);'`.

## Constraints
- `crypto.erl` is the ONLY public Erlang entry point; `crypto_ec_curves.erl` stays internal (`-moduledoc false`) and must not gain NIF or side-effecting calls.
- NIF ABI version: `-define(CRYPTO_NIF_VSN, 302)` (crypto.erl:815) MUST stay in lockstep with the C side, which rejects any other value (`c_src/crypto.c`, `if (vernum != 302)`).
- `on_load/0` loads the NIF from `code:priv_dir(crypto)/lib/crypto` (optionally suffixed by build type / system architecture) with the arg tuple `{?CRYPTO_NIF_VSN, LibBin, FipsMode}`; any NIF function not yet loaded falls back to `nif_stub_error/1` and raises `{nif_not_loaded, ...}`.
- Every call into the NIF goes through the `?nif_call` macro, which converts C errors into Erlang exceptions and adds argument info; new NIFs must be routed through it, not called raw.
- Error convention: older style raises bare `error:badarg` / `error:notsup`; newer style raises the 3-tuple `error:{Tag, {C_file,C_line}, Description}` with `Tag = badarg | notsup | error`; `format_error/2` builds the shell-friendly version from the 3-tuple.
- FIPS mode (env `fips_mode`, read at load) gates/disables algorithms — several API functions check `info_fips()` before proceeding (e.g. SRP); keep such gating when adding algorithms.
- The NIF stubs near the end of `crypto.erl` (`*_nif(...) -> ?nif_stub`) and the `-nifs([...])` list must stay consistent with the C implementations in `../c_src/` — a mismatch surfaces only at load/call time.
- `-Werror` is on: any warning (unused var/function, etc.) breaks the build.
- Types exported via `-export_type` are part of the public contract with `public_key`/`ssl`; changes ripple to those apps.
- Never hand-edit `ebin/crypto.app`/`crypto.appup`; edit the `.src` files.

## Design Decisions
- Data larger than `?MAX_BYTES_TO_NIF` (20000 bytes) is chunked in Erlang (`hash/2`, `hash_update/2`, ...) before crossing the NIF boundary, to bound NIF scheduling time / reduction cost.
- The NIF VSN is passed at load time and validated in C rather than hidden in an atoms table, giving a hard incompatibility check between `.beam` and `.so`.

## Notes for Agents
- Module/API documentation is INLINE here (`-moduledoc` and `-doc` attributes); `../doc/` holds only prose guides (`crypto_app.md`, `notes.md`, `guides/`, `docs.exs`) — do not look for per-function reference docs as separate files.
- `?CRYPTO_VSN` is undefined during doc generation, so `version/0` falls back to `"??"` via the `-ifndef(CRYPTO_VSN)` guard.
- The NIF C sources live in `../c_src/` (entry point `crypto.c`, new-API glue `api_ng.c`, atom/vsn table `atoms.c`); `Makefile` here does not compile them.
- Functions marked `-removed`/`-deprecated` are declared explicitly; only a small set is intentionally exported for tests.

## Test Strategy
- Common Test suites live in `../test/` (`crypto_SUITE` and friends); this `src/` dir has no tests of its own.
- The NIF must be built before Erlang tests run (tests exercise the real libcrypto), and results depend on the linked OpenSSL version — `info_lib/0` / `supports/0` are used to guard version-specific assertions.

## See Also
- `../c_src/` — NIF C implementation the modules load (owns the other half of the NIF contract).
- `../test/` — Common Test suites.
- `../include/` — public headers (`crypto.hrl` etc.) shared with other apps.
- `../doc/` — user guide / prose documentation.
