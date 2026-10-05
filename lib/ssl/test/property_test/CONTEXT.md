# SSL Property-Based Tests (EQC / PropEr)

## Intent
Property-based (QuickCheck-style) tests for the `ssl` application internals.
Holds the property modules and their data generators; they are compiled and executed by `ct_property_test` from the sibling Common Test suite `../ssl_eqc_SUITE.erl`.
The directory name `property_test` is mandated by `ct_property_test:init_per_suite/1` (it looks up that exact subdirectory next to the suite).
White-box tests: the modules `include_lib` ssl's internal `src/*.hrl` headers and drive ssl internal functions directly.

## API Surface
All modules `-compile(export_all)`; only the `prop_*` functions are invoked by the suite. Generators are grouped as `tls_version/0`, message generators (`tls_msg/1`), per-extension generators, and option/chain builders.

### `ssl_eqc_handshake.erl` (~1054 lines) — pure encode/decode, no network
- `prop_tls_hs_encode_decode/0` — round-trip property: `tls_handshake:encode_handshake/2` then `tls_handshake:decode_handshake/3` must return the original handshake record (for TLS 1.0–1.3 message types).
- `prop_hello_ext_unwrap/0` — `ssl_handshake:extension_value/1` must succeed (not crash) on every generated wrapped extension value.
- Generators cover TLS 1.0–1.3 handshake messages, all ClientHello/ServerHello/EncryptedExtensions/HelloRetryRequest extensions, key-share entries (incl. hybrid PQC groups `x25519mlkem768`, `secp256r1mlkem768`, `secp384r1mlkem1024` and ML-KEM/brainpool curves), signature schemes, PSK identities/binders and certificate chains.
- `raw_handshake/1` + `raw_ext_client/1`/`raw_ext_server/1` "unset" fields that decode fills in but that are not part of the wire record (`binder_length`, split hybrid key_exchange), so the encode/decode comparison is meaningful.

### `ssl_eqc_cipher_format.erl` (~298 lines) — pure string/format checks, no network
- `prop_tls_cipher_suite_rfc_name/0` — `ssl:str_to_suite(ssl:suite_to_str(S)) == S`.
- `prop_tls_cipher_suite_openssl_name/0` — `ssl:str_to_suite/1` of the openssl name round-trips to `S`, and the openssl name is either a `TLS_`-prefixed name or a known legacy OpenSSL name.
- `prop_tls_anon_cipher_suite_rfc_name/0` and `prop_tls_anon_cipher_suite_openssl_name/0` — same round-trips restricted to anonymous suites (pre-TLS-1.3 only).
- `prop_tls_signature_algs/0` — `ssl_cipher:signature_algorithm_to_scheme/1` of a generated `#'SignatureAlgorithm'{}` is a member of the expected scheme list.
- Local `openssl_legacy_names/0` lists the OpenSSL legacy suite names the app supports.

### `ssl_eqc_chain.erl` (~381 lines) — real TLS/DTLS handshakes, cert-chain edge cases
- `prop_tls_unordered_path/1` — `PrivDir` arg; unordered certificate chain (DER and PEM variants).
- `prop_tls_extraneous_path/1` — `PrivDir` arg; chain containing extraneous certificates (DER and PEM variants).
- `prop_tls_extraneous_paths/0` — extraneous chains, DER only.
- `prop_tls_extraneous_and_unordered_path/0` — combined extraneous + unordered chain, DER only.
- `prop_client_cert_auth/0` — client certificate authentication with varied key algorithms.
- All share `signature_algs_test/2`: run `ssl_test_lib:basic_test/3` with generated client/server options and assert it does not raise.

### `ssl_eqc_ecc.erl` (~156 lines) — real TLS/DTLS handshakes over many curves
- `prop_tls_ecc/0` — ECC curve + digest combinations for TLS 1.3 / 1.2 / DTLS 1.2 via `ssl_test_lib:basic_test/3`.
- Curves filtered by `crypto:supports(curves)` and `ssl_test_lib:sufficient_crypto_support/1`; edwards/brainpool name-translation helpers map (key-exchange group → signing curve) and TLS-1.3 suffix names.

## Constraints
- Compiled at suite runtime by `ct_property_test:compile_tests/2` using `make:all/1` with cwd set to this directory — there is **no `-I` include path passed**; `include_lib("ssl/src/*.hrl")` resolves via `code:lib_dir(ssl)`, so the ssl app (release install, `TEST_NEEDS_RELEASE=true`) must be on the code path with its `src/` headers present.
- Tool selection is by availability: `eqc` > `proper` > `triq` (override with `{prop_tools, [...]}` in Common Test `Config`). `ct_property_test` compiles with `-d EQC` / `-d PROPER` / `-d TRIQ`, and `common_test/include/ct_property_test.hrl` pulls in the matching prelude.
- `proper` is a supported fallback for `eqc` (the suite comments state eqc may be replaced by proper); generators rely only on the common prelude (`?FORALL`, `?LET`, `?SUCHTHAT`, `oneof`, `frequency`, `elements`, `choose`, `collect`) and on `rand`/`crypto`, not on eqc-only APIs.
- Randomness: generators call `crypto:strong_rand_bytes/1` and `rand:uniform/1`, so runs are not reproducible without a tool-provided seed.
- `prop_tls_*` functions never fail loudly: on exception they return `false`, letting the property tool shrink to the failing case rather than crashing the suite.

## Notes for Agents
- Not listed in `lib/ssl/test/Makefile` `MODULES`; the whole directory is shipped into the release test tree as `ssl_test/property_test` (`release_tests_spec`) and compiled there by `ct_property_test`. Do not add a per-file module list or a Makefile here.
- `ssl_eqc_handshake` and `ssl_eqc_cipher_format` are pure (no sockets, no network) and can be run standalone, e.g. `eqc:quickcheck(ssl_eqc_handshake:prop_tls_hs_encode_decode()).`
- `ssl_eqc_chain` and `ssl_eqc_ecc` open real TLS/DTLS connections (`ssl_test_lib:basic_test`) and call `ssl:start()`; the ones taking a `PrivDir` argument write PEM/chain files there, so they need a writable priv dir.
- To run the whole set under the suite: `make test` / `ct_run` in `lib/ssl` (tests need an installed release).

## Routing Table
| Module | Theme |
| --- | --- |
| `ssl_eqc_handshake.erl` | TLS 1.0–1.3 handshake message + extension encode/decode round-trips |
| `ssl_eqc_cipher_format.erl` | Cipher-suite name formats (RFC/OpenSSL) and signature-algorithm mapping |
| `ssl_eqc_chain.erl` | Certificate-chain building/validation edge cases over live TLS/DTLS |
| `ssl_eqc_ecc.erl` | ECC curve/digest combinations over live TLS/DTLS |
| `../ssl_eqc_SUITE.erl` | Common Test driver that invokes the `prop_*` functions (sibling — read-only, escalate writes to parent) |

## Test Strategy
- Driven by `ssl_eqc_SUITE` test cases, one per property, via `ct_property_test:quickcheck(Module:prop_xxx(), Config)`.
- `tls_unorded_chains`, `tls_extraneous_chain`, `tls_client_cert_auth`, `tls_eccs` pass `PrivDir` from `Config` and call `ssl:start()`.
- If no property tool is installed the suite is skipped ("No property testing tool found"); there is no OSS default dependency on QuickCheck (eqc is commercial, PropEr is the practical fallback).

## See Also
- `../ssl_eqc_SUITE.erl` — the Common Test suite that orchestrates these properties.
- `../../Makefile` (`TEST_NEEDS_RELEASE=true`), `../Makefile` (release_tests_spec ships `property_test/`).
- `../../../common_test/src/ct_property_test.erl` and `../../../common_test/include/ct_property_test.hrl` — the tool-agnostic driver and prelude.
