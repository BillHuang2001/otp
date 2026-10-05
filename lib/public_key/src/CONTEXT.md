# public_key — Erlang sources

## Intent
Erlang source directory of the OTP `public_key` application (app version `PUBLIC_KEY_VSN` in `../vsn.mk`, currently `1.21.7`).
Contains three kinds of code: the single public API module (`public_key`), the internal `pubkey_*` implementation modules, and the ASN.1-generated modules that the sibling `../asn1/` build emits into this directory.
Build/test/release is driven by `src/Makefile` (plus the parent `../Makefile`); the app resource templates are `public_key.app.src` / `public_key.appup.src`.

## API Surface — hand-written modules
`public_key` (3424 lines) — the only public module.
- PEM/ASN.1 API: `pem_decode/1`, `pem_encode/1`, `der_decode/2`, `der_encode/2`, `pem_entry_decode/1,2`, `pem_entry_encode/2,3`, `pkix_decode_cert/2`, `pkix_encode/3`.
- Certificate API: `pkix_is_self_signed/1`, `pkix_is_fixed_dh_cert/1`, `pkix_is_issuer/2`, `pkix_issuer_id/2`, `pkix_subject_id/1`, `pkix_normalize_name/1`, `pkix_path_validation/3`, `pkix_verify_hostname/2,3`, `pkix_verify_hostname_match_fun/1`, `pkix_sign/2`, `pkix_verify/2`, `pkix_hash_type/1`, `pkix_sign_types/1`.
- Certificate revocation: `pkix_crls_validate/3`, `pkix_crl_verify/2`, `pkix_crl_issuer/1`, `pkix_dist_point/1`, `pkix_dist_points/1`, `pkix_match_dist_point/2`, `short_name_hash/1`, `pkix_ocsp_validate/5`, `ocsp_extensions/1`.
- Key API: `generate_key/1`, `compute_key/2,3`, `dh_gex_group/4`, `dh_gex_group_sizes/0`, `sign/3,4`, `verify/4,5`, plus the legacy RSA-only `encrypt_private/decrypt_private/encrypt_public/decrypt_public` (2- and 3-arity).
- OS trust store: `cacerts_get/0`, `cacerts_load/0,1`, `cacerts_clear/0`.
- Test-data generators (exported, documented under "Test Data API"): `pkix_test_data/1`, `pkix_test_root_cert/2`.
- SSHCURVE mapping: `ssh_curvename2oid/1`, `oid2ssh_curvename/1`.
- Tracing hook: `handle_trace/3` (used by `pubkey_ocsp`/`pubkey_cert` error reporting paths).
- Exports ~40 opaque/typed API types via `-export_type` (`public_key/0`, `private_key/0`, `cert/0`, `pem_entry/0`, `oid/0`, `policy_node/0`, RSA/DSA/ECDSA/EdDSA/ML-DSA/ML-KEM/SLH-DSA key types, `test_config/0`, `test_root_cert/0`).
Internal modules (all `-moduledoc false`, called only by `public_key` and each other; do not treat as API):
- `pubkey_cert` (2275 lines) — RFC 5280 path/certificate validation core: `init_validation_state/3`, `validate_extensions/4`, `validate_time/3`, `validate_issuer/4`, `validate_names/6`, `validate_signature/6`, name/ID helpers (`normalize_general_name/1`, `match_name/3`, `select_extension/2`), and test-cert helpers `gen_test_certs/1`, `root_cert/2`, `x509_pkix_sign_types/1`.
- `pubkey_cert_records` (651 lines) — decodes ASN.1 cert records into the `OTP*` record set and maps algorithm/curve OIDs (`decode_cert/1,2`, `transform/2`, `supportedPublicKeyAlgorithms/1`, `oid_to_ml_dsa_algo/1`, `oid_to_ml_kem_algo/1`, `oid_to_slh_dsa_algo/1`, `encode_extensions/1`, `decode_extensions/1`).
- `pubkey_translation` (110 lines) — `decode/1`/`encode/1`, the OID→atom and record→`OTP*` translation layer used by `pubkey_cert_records`.
- `pubkey_crl` (713 lines) — CRL/DPCRL validation: `validate/7`, `init_revokation_state/0`, `fresh_crl/3`, `verify_crl_signature/4`, `is_delta_crl/1`, `combines/2`, `match_one/2`.
- `pubkey_ocsp` (353 lines) — OCSP response parsing/verification: `find_single_response/3`, `status/2`, `verify_response/5`, `decode_response/1`, `get_nonce_extn/1`, `get_acceptable_response_types_extn/0`; enforces a 100 KB max response size (`MAX_OCSP_RESPONSE_SIZE`).
- `pubkey_policy_tree` (429 lines) — certificate policy tree construction/pruning (`add_leaves/3`, …); exports the `policy_tree/0` type.
- `pubkey_pem` (300 lines) — PEM text framing and symmetric ciphers for encrypted PEM: `encode/1`, `decode/1`, `cipher/3`, `decipher/2`.
- `pubkey_pbe` (335 lines) — PBES1/PBES2 password-based encryption: `encode/4`, `decode/4`, `decrypt_parameters/1`, `encrypt_parameters/1`, `pbdkdf1/4`, `pbdkdf2/7`.
- `pubkey_os_cacerts` (330 lines) — OS/`cacerts` trust-store loading (`load/0,1`, `get/0`, `clear/0`, `format_error/2`); loads the `../c_src` NIF for the Windows store via `erlang:load_nif/2`.
- `pubkey_ssh` (79 lines) — only `dh_gex_group/4` and `dh_gex_group_sizes/0` remain; the SSH key encode/decode parts were moved to the `ssh` application.

## Generated files (never hand-edit)
- All ASN.1 modules/hrl (`OTP-PKIX`, `OTP-PKIX-Relaxed`, `PKCS-FRAME`, `PKCS-1/3/10`, `PKIX*`, `X509-ML-DSA-2025`, `X509-ML-KEM-2025`, `SLH-DSA-Module-2024`, `DSS`, `ECPrivateKey`, `RFC5639`, `Safecurves-pkix-18`, …) are emitted **into this directory** by `../asn1/Makefile` (`ESRC = ../src`, `erlc -bber +der +noobj -o ../src`), from the `.asn1` sources in `../asn1/`. (There is no generated `OTP-PUB-KEY` module — the `OTP-PUB-KEY.*` entries in `../.gitignore` are stale; `../include/OTP-PUB-KEY.hrl` is a hand-written shim.)
- Those generated `*.erl`/`*.hrl` plus `src/*.asn1db` are all git-ignored (see `../.gitignore`) — they are absent from a fresh checkout and appear only after building.
- `pubkey_moduli.hrl` (904 lines / ~700 KB) is generated from `../priv/moduli` by `../priv/convert.escript` (Makefile rule `pubkey_moduli.hrl: ../priv/moduli`) and, unlike the ASN.1 output, **is tracked in git** — regenerate it, never edit or reformat it.
- `../include/PKCS-FRAME.hrl` is a copy of the generated `src/PKCS-FRAME.hrl`, produced by the `$(INCLUDE)/%.hrl: $(ESRC)/%.hrl` rule in `../asn1/Makefile`.

## Headers
- Public headers live in `../include/` (`public_key.hrl`, `OTP-PUB-KEY.hrl`, `PKIXCMP.hrl`, `PKIXCRMF.hrl`) — these are the ones users `-include_lib`.
- `public_key_internal.hrl` (323 lines) is the internal header: it includes the generated ASN.1 hrl files (with `-undef` fixes for colliding macro names) and defines internal records (`path_validation_state`, `revoke_state`, `cert`, `ECPoint`, ML-DSA/ML-KEM/SLH-DSA key records, `PBEParameter`/`PBES2-params`, …) and legacy OID macros not present in modern specs.
- `pubkey_moduli.hrl` holds `?dh_default_groups` and is included only by `pubkey_ssh.erl`.

## Constraints
- Any new/removed module must be updated in three places: the `MODULES` list in `src/Makefile`, the `modules` list in `public_key.app.src` (which also lists all generated ASN.1 modules), and `../.gitignore` if it is generated.
- Nothing in `src/` may be added to git if it is a build artifact (`*.beam`, generated `*.erl`/`*.hrl`/`*.asn1db`).
- Internal modules are not part of the public API: keep them `-moduledoc false` and free to change; only `public_key` (and the `../include` headers) is a supported interface.
- Compilation requires `-I ../include -I ../asn1/` (set as `PUB_KEY_ERL_FLAGS`) so that modules can include the generated headers before/after they exist.
- Erlang/OTP coding style applies (`sys_pre_attributes` parse transform, `+warn_obsolete_bool_op`, `-doc`/`-moduledoc` EEP-48 doc attributes in `public_key`).

## Notes for Agents
- Because the ASN.1 output is not committed, `rg`/`git grep` on this directory shows only the hand-written modules and `pubkey_moduli.hrl`; a fully built tree has ~30 additional generated modules here.
- `public_key:pkix_*` functions consume the `OTP*` records from `../include/public_key.hrl`, not the raw ASN.1 records — `pubkey_translation`/`pubkey_cert_records` perform that conversion and are the place to look for OID/field mapping questions.
- `public_key.erl` also carries `-removed([...])` entries for the `ssh_*` functions that moved to `ssh`, and `-unsafe([...])` markers on the legacy RSA encrypt/decrypt functions (this is an xref "unsafe function" annotation: `possibly` means unsafe only with attacker-influenced input; see `pubkey_ocsp`/`pubkey_cert` for hardening measures such as the OCSP response size cap).
- Compilation through `src/Makefile` inserts an `app_vsn` attribute via `+'{attribute,insert,app_vsn,$(APP_VSN)}'`; a bare `erlc` invocation skips this and will not reproduce release builds.
- `public_key.app.src` / `public_key.appup.src` are templates: `%VSN%` is substituted by sed into `../ebin/public_key.app[up]`.
- `-moduledoc`/`-doc` content in `public_key.erl` is grouped (`Keys`, `ASN.1 Encoding API`, `PEM API`, `Certificate API`, `Certificate Revocation`, `Sign/Verify API`, `Key API`, `Legacy RSA Encryption API`, `Test Data API`, `Common`) and feeds the ExDoc build in `../doc/` — keep group names consistent when adding functions.

## Dependencies
- Build time: the `asn1` application compiler plus this app's own `../asn1/` step (must run before `src/`); `asn1` and `crypto` are also the dialyzer PLT apps (`DIA_PLT_APPS` in `../Makefile`).
- Runtime: `asn1`, `crypto`, `kernel`, `stdlib` (declared in `public_key.app.src`); crypto supplies all key operations and the ML-DSA/ML-KEM/SLH-DSA primitives.
- `pubkey_os_cacerts` depends on the `../c_src` NIF (Windows certificate store); on other platforms it falls back to reading PEM files.

## Test Strategy
- All tests live in the sibling `../test/` directory (Common Test suites such as `pkits_SUITE`, `pbe_SUITE`, driven by `../test/Makefile`); no tests are kept beside the sources.
- Certificate-validation behaviour is exercised against the PKITS/NIST fixture set and `erl_make_certs.erl` test helpers under `../test/`.

## See Also
- `..` (`lib/public_key/`) — app Makefile, `vsn.mk`, `include/`, `priv/`, `c_src/`, `doc/`, `test/`.
- `../asn1/` — ASN.1 specifications and generation rules that produce this directory's generated modules.
- `../../crypto/` and `../../asn1/` — runtime/build dependencies; `../../ssl/` and `../../ssh/` are the main consumers of `public_key`.
