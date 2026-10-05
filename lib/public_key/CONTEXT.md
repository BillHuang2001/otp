# public_key — Public Key Infrastructure

## Intent
OTP application implementing public-key infrastructure: X.509 certificate/CRL handling (decode/encode, RFC 5280 path validation), digital signatures and sign/verify helpers, key generation and key agreement, PEM and PKCS#8/PKCS#10/CMS/OCSP/CMP/CRMF packaging.
It is a pure library: it does not read or write files (functions take/return binaries) except the OS trust-store functions `cacerts_load/0,1` and `cacerts_get/0`.
It sits above `crypto` (all cryptographic operations, including ML-DSA/ML-KEM/SLH-DSA) and uses the `asn1` compiler to generate its encode/decode backends from the ASN.1 specs in `asn1/`.
App version `PUBLIC_KEY_VSN` lives in `vsn.mk` (currently `1.21.7`); the app resource is `src/public_key.app.src` / `src/public_key.appup.src`.
`Makefile` builds `SUB_DIRECTORIES = asn1 src c_src doc` (in that order: ASN.1 modules must exist before the Erlang sources compile); `test` is deliberately not in `SUB_DIRECTORIES` (run separately).

## API Surface
The public module is `public_key` (`src/public_key.erl`), whose functions are grouped by the doc tags:
- ASN.1/PEM: `pem_decode/1`, `pem_encode/1`, `der_decode/2`, `der_encode/2`, `pem_entry_decode/1,2`, `pem_entry_encode/2,3`, `pkix_decode_cert/2`, `pkix_encode/3`.
- Certificate: `pkix_is_self_signed/1`, `pkix_is_fixed_dh_cert/1`, `pkix_is_issuer/2`, `pkix_issuer_id/2`, `pkix_subject_id/1`, `pkix_normalize_name/1`, `pkix_path_validation/3`, `pkix_verify_hostname/2,3`, `pkix_verify_hostname_match_fun/1`, `pkix_sign/2`, `pkix_verify/2`, `pkix_hash_type/1`, `pkix_sign_types/1`.
- Certificate revocation: `pkix_crls_validate/3`, `pkix_crl_verify/2`, `pkix_crl_issuer/1`, `pkix_dist_point/1`, `pkix_dist_points/1`, `pkix_match_dist_point/2`, `short_name_hash/1`, `pkix_ocsp_validate/5`, `ocsp_extensions/1`.
- Keys: `generate_key/1`, `compute_key/2,3`, `sign/3,4`, `verify/4,5`, `dh_gex_group/4`, `dh_gex_group_sizes/0`, plus the legacy RSA-only `encrypt_private/decrypt_private/encrypt_public/decrypt_public` (marked `-unsafe`, superseded by `public_key:crypto`-level ops).
- OS trust store: `cacerts_get/0`, `cacerts_load/0,1`, `cacerts_clear/0`.
- Test-data generators: `pkix_test_data/1`, `pkix_test_root_cert/2`; SSHCURVE mapping `ssh_curvename2oid/1`, `oid2ssh_curvename/1`; tracing hook `handle_trace/3`.
It exports ~40 API types via `-export_type` (`public_key/0`, `private_key/0`, `cert/0`, `pem_entry/0`, `oid/0`, RSA/DSA/ECDSA/EdDSA/ML-DSA/ML-KEM/SLH-DSA key types, `policy_node/0`, `test_config/0`, `test_root_cert/0`).
Cross-app contract: `include/public_key.hrl` is `-include_lib`'d by `ssl`, `ssh`, `inets`, `tools`, `ftp` and `compiler` tests — its records/OID macros are the stable interface.

## Constraints
- Never hand-edit generated code: the ASN.1 encoder/decoder modules are produced by `asn1/` into `src/` (git-ignored); `src/pubkey_moduli.hrl` is generated from `priv/moduli` by `priv/convert.escript`.
- Only `public_key` and the `include/*.hrl` headers are a supported API; the `pubkey_*` modules are internal (`-moduledoc false`) and free to change.
- Keep `asn1/Makefile`'s `.NOTPARALLEL:` — the RFC 5912 modules import each other cyclically and parallel compilation makes the generated atom names non-reproducible.
- Records in `include/public_key.hrl` must round-trip with the generated encoders (keep `asn1_NOVALUE`/`asn1_DEFAULT` defaults); changing/removing a field or OID macro is an OTP-wide backward-incompatible change.
- Adding an app module requires updating `MODULES` in `src/Makefile`, the `modules` list in `src/public_key.app.src`, and `.gitignore` if generated.

## Design Decisions
- ASN.1 is the source of truth: X.509/PKIX (RFC 5912), PKCS, CMS, OCSP, CMP, CRMF and the post-quantum key encodings are declared in `asn1/*.asn1`, and `erlc -bber +der` generates the BER backend plus DER `decode/encode` functions that `public_key:der_encode/der_decode` dispatch to.
- Two certificate record representations coexist: the strict ASN.1 records (`'Certificate'`, `'TBSCertificate'`, ...) and the OTP "relaxed" records (`'OTPCertificate'`, `'OTPTBSCertificate'`, `'OTPSubjectPublicKeyInfo'`, ...); `pubkey_cert_records`/`pubkey_translation` convert between them, and the public API exposes the `OTP*` form.
- `OTP-PKIX-Relaxed.asn1` exists so `pkix_decode_cert/2`'s `relaxed` option can decode real-world certs (empty/zero-length RDNs, over-long CN, alternative string types).
- Certificates are validated against a lazily-built chain built from `include/public_key.hrl` records; `pubkey_cert` holds the RFC 5280 path-validation state machine and `pubkey_policy_tree` the certificate-policy tree.

## Dependencies
- Runtime: `asn1`, `crypto`, `kernel`, `stdlib` (declared in `public_key.app.src`); `crypto` provides every key/signature primitive.
- Build time: the `asn1` application compiler plus this app's own `asn1/` step; `asn1` and `crypto` are the dialyzer PLT apps (`DIA_PLT_APPS`).
- Platform: the Windows certificate-store NIF (`c_src/`) requires a `win32` target build; other platforms use PEM fallbacks.

## Notes for Agents
- A fresh checkout has only the hand-written sources; the ~30 generated ASN.1 modules appear in `src/` after a build.
- `priv/moduli` → `src/pubkey_moduli.hrl` generation is a build step; the `moduli` data file itself is regenerated manually via `priv/generate`.
- There is no generated `OTP-PUB-KEY` module — `include/OTP-PUB-KEY.hrl` is a hand-written shim that `-include`s `public_key.hrl` (the `OTP-PUB-KEY.*` `.gitignore`/cover entries are stale).

## Test Strategy
- Common Test suites live in `test/` and run against the built app; `make test` in this dir (or top-level `make public_key_test`) runs them, narrowed via `ARGS="-suite <S> [-case <C>]"`.
- `pkits_SUITE` (NIST PKITS vectors) and `public_key_SUITE` dominate runtime; `pubkey_eqc_SUITE` needs an external QuickCheck/PropEr.
- Fixture-driven: `test/*_SUITE_data/` PEM/DER fixtures plus generated certs from `test/erl_make_certs.erl`.

## Routing Table
- `src/` → Erlang sources: public `public_key` module, internal `pubkey_*` modules, and the git-ignored ASN.1-generated modules emitted here.
- `asn1/` → ASN.1 specifications (RFC 5912/PKIX/PKCS/CMS/OCSP/CMP/CRMF/PQC) and the generation rules that produce the code in `src/` and `include/`.
- `include/` → shipped public headers (`public_key.hrl`, `OTP-PUB-KEY.hrl`, `PKIXCMP.hrl`, `PKIXCRMF.hrl`) shared across OTP.
- `test/` → Common Test suites, fixture data and `erl_make_certs` helper.
- `doc/` → ExDoc/markdown documentation sources and guides (API reference is generated from `src/` `-doc` attributes).
- `c_src/` → Windows certificate-store NIF for `pubkey_os_cacerts`.
- `priv/` → DH `moduli` data, its generators (`convert.escript`, `generate`) and the NIF build-output dir.
- `ebin/` → build output only (git-ignored).

## See Also
- `../crypto/` (cryptographic primitives), `../asn1/` (ASN.1 compiler), `../ssl/` and `../ssh/` (largest consumers of `public_key.hrl`).
- `../CONTEXT.md` — the OTP library apps as a whole; repo-root `CONTEXT.md` for top-level build/release entry points.
