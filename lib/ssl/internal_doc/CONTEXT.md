# ssl/internal_doc

## Intent
Developer-facing notes for the `ssl` application: implementation-level design notes that are deliberately NOT published documentation.
The content exists so a maintainer does not have to re-derive the OCSP-stapling data model, the test-certificate topology or the PEM/cert cache table relations from the code.
This directory is a working notebook for `../src/` and `../test/` — it is not built, not installed and not rendered by ExDoc.

## API Surface
- `ssl_notes.md` — the only file (plain markdown + mermaid, no build step, no generated counterpart). Three independent sections:
  1. `client-side OCSP stapling` — three-level conceptual model `stapling` (user option) → `stapling_state` (per-handshake state) → `stapling_info` (data needed to verify the chain), with a mermaid class diagram.
  2. `ssl test certificates` — CA/leaf topology produced by `../test/make_certs.erl`, as a mermaid flowchart.
  3. `Notes on the PEM and cert caches` — ASCII data-relation diagram plus per-table descriptions (`PemCache`, `FileMapDb`, `RefDb`, `CertDb`); implemented in `../src/ssl_pem_cache.erl` and `../src/ssl_pkix_db.erl`, driven by `../src/ssl_manager.erl`.

## Constraints
- Developer notes only: `../doc/docs.exs` never lists it, `make/ex_doc.exs` only wildcards `internal_docs/*.md` (plural) and `make/doc.mk` only wildcards inside `doc/` — so editing this file cannot affect generated HTML/man docs, `make install` output or the release.
- The file must keep its HTML-comment SPDX/Apache-2.0 header block: repo markdown files are license-header checked.
- Apps that do publish internals (e.g. `lib/compiler/doc/docs.exs`) add them explicitly to `extras`; `ssl` does not, so anything here stays private to the source tree.

## Notes for Agents
### stapling note → real code (names in the note are conceptual, not literal)
- `stapling` user option is `staple | no_staple | map()` (default `no_staple`, client-only); `ssl_config:opt_stapling/3` normalizes it to the internal map `#{ocsp_nonce => boolean()}` — stapling is enabled iff the `stapling` key is present in ssl options (`?DEFAULT_STAPLING_OPT`/`?DEFAULT_OCSP_NONCE_OPT` in `../src/ssl_internal.hrl`).
- `stapling_state` is `#handshake_env.stapling_state` (`../src/ssl_connection.hrl`), defaulting to `#{configured => false, status => not_negotiated}`.
- The real keys are `configured`, `status` (`not_negotiated | negotiated | not_received | received_staple`), `ocsp_nonce` (DER-encoded 32-byte nonce from `tls_handshake:ocsp_nonce/1`, RFC 8954) and `staple` (the received `#certificate_status{}`). The note calls that last field `response`, a name that does not exist in the code.
- `stapling_info` exists nowhere in the codebase; it maps to the `cert_ext` key threaded into the cert-validation UserState — `#{cert_ext => #{SubjectId => [#certificate_status{}]}, stapling_state => ...}`, built by `ext_info/2` in `../src/tls_dtls_client_connection.erl` and `../src/tls_dtls_server_connection.erl` (the value is a *list* of status records, so the note's bare `Status` is imprecise).
- Consumers: `ssl_certificate:verify_cert_extensions/4` verifies the staple via `public_key:pkix_ocsp_validate/5`; `ssl_handshake:cert_status_check/5` hard-fails when stapling was configured but no staple arrived (no CRL/OCSP fallback).
- Status transitions live in `../src/tls_handshake.erl` (TLS 1.2 ServerHello `status_request` extension), `../src/tls_dtls_client_connection.erl` (`wait_stapling` state) and `../src/tls_handshake_1_3.erl`; validation UserState is assembled in `../src/ssl_handshake.erl` (`path_validation/9`).

### test-cert topology note
- `../test/make_certs.erl` (`all/3`) creates root `erlangCA` → intermediate `otpCA` → leaves `client`, `server`, `revoked`, `undetermined`, `a.server`, `b.server`; `localhost` is issued directly by `erlangCA`; `revoked` is revoked and `undetermined` is removed from the CA database.
- `b.server` is regenerated with `cert_profile = "user_cert_ocsp_signing"` only when the `staple` config flag is set.
- The numbered / big-serial labels in the flowchart (`1:otpCA`, `2:localhost`, `3:server`, `BIG_RAND_SERIAL:erlangCA`) appear nowhere in the code — they denote extra cert variants produced by suites invoking `make_certs` with different configs, not names to grep for.

### stale cross-reference
- `../src/ssl_pem_cache.erl`, `../src/ssl_manager.erl` and `../test/ssl_pem_cache_SUITE.erl` still comment "see `internal_doc/pem_and_cert_cache.md`"; that file no longer exists — the PEM/cert-cache content lives in `ssl_notes.md`.

## Routing
- No subdirectories.
- Read-only cross-references (writes belong to those nodes, escalate to `../`):
  - `../src/` — the implementation these notes describe (`ssl_handshake.erl`, `ssl_certificate.erl`, `tls_dtls_client_connection.erl`, `tls_handshake*.erl`, `ssl_config.erl`, `ssl_pem_cache.erl`, `ssl_pkix_db.erl`).
  - `../test/` — `make_certs.erl` and `ssl_pem_cache_SUITE.erl` / `openssl_stapling_SUITE.erl`, the helpers/suites the notes document.
  - `../doc/` — published ssl documentation (separate from this directory; do not confuse the two).

## See Also
- `../CONTEXT.md` — the `ssl` application node (build, options, test layout).
