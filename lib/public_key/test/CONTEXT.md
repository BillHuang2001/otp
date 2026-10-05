# public_key — Common Test Suites

## Intent
Common Test suites, fixture data and helpers verifying the OTP `public_key` application (`../src/`).
Tests run the *built* app (`-pa $(ERL_TOP)/lib/public_key/ebin`) and are data-driven from the `*_SUITE_data/` PEM/DER fixtures.
`test` is NOT part of `SUB_DIRECTORIES` in `../Makefile` (`asn1 src c_src doc`); the suites are compiled and run separately (see Test Strategy).

## API Surface (per file in `MODULES` of `Makefile`)
- `public_key_SUITE.erl` — the main suite (~171 KB, 88 test cases): app/appup sanity, PEM decode/encode, encrypt/decrypt (+ custom `sign_fun`), ext encoding, sign/verify (rsa, rsa_pss, dsa, ecdsa, eddsa, ML-DSA, SLH-DSA), PKIX decode/encode, path validation (incl. forged chains, expired roots, key usage), hostname verification, OpenSSL-style issuer hash, pkcs7/pkcs10, ssh-key helpers. Groups: `pem_decode_encode`, `sign_verify`, `explicit_ec_params`.
- `pkits_SUITE.erl` — NIST PKITS path-validation vectors (~122 KB, 103 test cases in 13 groups: signature_verification, validity_periods, verifying_name_chaining, verifying_paths_with_self_issued_certificates, basic_certificate_revocation_tests, delta_crls, distribution_points, verifying_basic_constraints, key_usage, name_constraints, private_certificate_extensions, policies, …).
- `pbe_SUITE.erl` — password-based encryption: `pbdkdf1`, `pbdkdf2`, `old_pbe`, `pbes1`, `pbes2`.
- `pubkey_cert_SUITE.erl` — `pubkey_cert` ASN.1 time conversions: UTCTime/GeneralizedTime ↔ gregorian UTC, including the 50-year window boundaries.
- `pubkey_policy_tree_SUITE.erl` — `pubkey_policy_tree` API: add_leaf(all)/siblings, all/any/valid/constrained policy node sets, prune leaves/tree/invalid nodes (debug helpers emit mermaid HTML into the test priv dir).
- `pubkey_ocsp_SUITE.erl` — `pubkey_ocsp` API + designated responder, responder cert expired / not-yet-valid; OCSP responses are hardcoded DER binaries inside the suite (no data dir).
- `pubkey_eqc_SUITE.erl` — property tests via `ct_property_test`, delegating to `property_test/pkix`; ASN.1 encode/decode round-trips for implicit/explicit/OCSP/algorithm/CRMF/CMP/CMS types. Timetrap 20 s per test case.
- `erl_make_certs.erl` — helper module (not a suite): generates RSA/DSA/EC keys and certificates, PEM↔DER conversions; used by `public_key_SUITE` and `pkits_SUITE`.
- `public_key.spec` — ct_run spec for the released tests (`suites` in `../public_key_test`, `cte_track` event handler, `cth_log_redirect`, `incl_app` details, `excl_mods` `OTP-PUB-KEY`/`PKCS-FRAME`).
- `public_key.cover` — cover configuration for the released test dir.

## Test Data
- `public_key_SUITE_data/` (61 files) — PEM fixtures: private/public keys and certs for rsa, rsa-pss, dsa, ec (incl. explicit params), eddsa, ML-DSA, ML-KEM, SLH-DSA; pkcs7/pkcs10, dh params, CRLs, hostname-verification certs (`verify_hostname.conf` variants). Several fixtures carry the generating openssl command in a comment.
- `pbe_SUITE_data/` (9 files) — PBE-encrypted keys (PBES1 MD5/DES, PBES2 RC2/3DES/AES-128/192/256) plus their expected decrypted keys.
- `pkits_SUITE_data/pkits/` (579 files) — NIST PKITS `certs/`, `crls/` and `smime-pem/` vectors (see the NIST PKITS spec linked in the suite header).
- `property_test/pkix.erl` — generators + `?FORALL` properties module used by `pubkey_eqc_SUITE`.

## Constraints
- Fixture layout is relied upon by `?config(data_dir, Config)`; `*_SUITE_data` and `property_test` are copied verbatim to `$(RELEASE_PATH)/public_key_test` by `release_tests_spec` (`RELSYSDIR = $(RELEASE_PATH)/public_key_test`) — new data dirs must be added to that tar step.
- Compile flags: `-I. -I ../include -pa $(ERL_TOP)/lib/public_key/ebin` plus `+nowarn_missing_spec_documented`; `+deterministic` is explicitly filtered out.
- Suites may `put/2` the data dir into the process dictionary in `init_per_testcase` (e.g. `pkits_SUITE`) — helpers read it from there.

## Notes for Agents
- `public_key_SUITE.erl` is a single huge module; find a test case by function name and check `groups/0` for group membership — never split/reformat it.
- The `explicit_ec_params` group and several cases are dead code unless the suite is compiled with `-DEXPLICIT_EC_PARAMS` (define is commented out in the source).
- `pubkey_eqc_SUITE` needs an external QuickCheck (eqc) or PropEr installation; without it the cases cannot run.
- `erl_make_certs` is compiled from `MODULES` but is not a suite — don't expect it in ct_run output.

## Dependencies
- A built `public_key` app (and `crypto`, `asn1`) in `$ERL_TOP`; `pkits_SUITE` skips unless `crypto:supports(hashs)` includes `sha256`, and DSA cases skip unless `dss` is in `crypto:supports(public_keys)`.
- Optional: QuickCheck/PropEr for `pubkey_eqc_SUITE`.
- openssl is not invoked at test time — only referenced in comments documenting how fixtures were produced.

## Test Strategy
- Run all suites of the app: `(cd $ERL_TOP/lib/public_key && make test)` or from `$ERL_TOP`: `make public_key_test`.
- Narrow the run with ct_run args: `... make test ARGS="-suite public_key_SUITE"` (optionally `-case <name>`).
- Release for out-of-tree testing: `./otp_build tests` then run from `$TESTROOT/public_key_test` (uses `public_key.spec`).
- `pkits_SUITE` and `public_key_SUITE` dominate runtime (large data-driven, 100+ and 88 cases); expect long runs.

## See Also
- `../src/` — modules under test (`pubkey_cert`, `pubkey_crl`, `pubkey_ocsp`, `pubkey_pbe`, `pubkey_pem`, `pubkey_policy_tree`, `pubkey_ssh`, `pubkey_translation`, `public_key`).
- `../include/` — `public_key.hrl`, `PKIX*.hrl` consumed by the suites.
- `../asn1/` — ASN.1 specs the encoded fixtures and PKIX records come from.
