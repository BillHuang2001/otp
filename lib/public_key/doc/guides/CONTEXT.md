# public_key — Documentation Guides

## Intent
Hand-written markdown user/reference guides for the `public_key` application, the "extra" pages (alongside `../public_key_app.md` and `../notes.md`) consumed by the release-wide ExDoc build.
There are exactly two files here: a records reference (`public_key_records.md`) and a usage examples / user's guide (`using_public_key.md`).

## API Surface
- `public_key_records.md` — "Public-Key Records": documents the Erlang records and OID name atoms derived from the ASN.1/PKIX specs that `public_key.hrl` exposes (types only, semantics deferred to the linked RFCs).
  - Sections: Data Types (non-standard base types `time()`, `general_name()`, `special_string()`, `dist_reason()`), RSA, DSA, ECDSA and EDDSA, PKIX Certificates, PKIXCMP Certificate Management Protocol, PKIXCRMF Certificate Request Message Format, Standard Certificate Extensions, Private Internet Extensions, CRL and CRL Extensions Profile (CRL Extensions, CRL Entry Extensions), PKCS#10 Certification Request.
  - Public explicit anchors (link targets): `#StdCertExt`, `#PrivIntExt`, `#CRLCertExt`, `#CRLEntryExt`, `#PKCS10` — do not rename/remove.
- `using_public_key.md` — "Examples": the user's guide with worked shell sessions.
  - Sections: PEM Files (DSA Private Key, RSA Private Key with Password, X509 Certificates, Encoding Public-Key Data to PEM Format), RSA Public-Key Cryptography, Digital Signatures, Verifying a certificate hostname (Background, The verification process, Function call examples, Extending the search mechanism → Hostname extraction + Re-defining the match operation, "Pinning" a Certificate).
  - Public explicit anchors (link targets): `#verify_hostname`, `#verify_hostname_examples`, `#hostname_extraction`, `#redefining_match_op`; the auto-generated heading anchor `#pinning-a-certificate` (from heading `"Pinning" a Certificate`) is also linked externally — keep that heading's text stable.

## Constraints
- Markdown only; `../html/`, `../man*/` and `../chunks/` under `../doc/` are generated build artifacts — never edit them here.
- Both files are wired as ExDoc `extras` in the parent `../docs.exs`, which is the only place their ordering is fixed; the shared `../../../make/ex_doc.exs` also auto-globs `guides/*.md`, so a new file here appears automatically (with default ordering unless added to `../docs.exs`).
- Do NOT duplicate API reference text (module/function docs) here — that is generated from `-doc`/`-moduledoc` attributes in `../src/*.erl`.
- Anchor names are a public contract: explicit anchors use `snake_case`/`CamelCase` while heading-derived anchors are `kebab-case` — renaming a file, heading, or anchor silently breaks external links.

## Cross-References (external links into these files)
- `../src/public_key.erl` links `public_key_records.md` (the records reference) and, in the hostname-verification docs, `using_public_key.md#verify_hostname`, `#verify_hostname_examples`, `#redefining_match_op`, `#pinning-a-certificate`, `#hostname_extraction`.
- `../../ssl/src/ssl.erl` and `../../ssl/src/ssl_crl_cache_api.erl` link `e:public_key:public_key_records.md` (X509 certificate records).

## Notes for Agents
- Keep the guide prose consistent with RFC 6125 terminology already used in `using_public_key.md` (Presented IDs vs Reference IDs, `fqdn_fun`, `match_fun`, `fail_callback`).
- The records guide is the canonical place to look up `#'...'{}` fields and `id-ce-*` OID atom/type mappings; the actual record definitions live in `../include/public_key.hrl`.

## Test Strategy
- No tests here; correctness is enforced at docs build time by ExDoc unresolved-reference/link warnings (`make docs` in `../doc/`).

## See Also
- `../CONTEXT.md` — the `public_key/doc/` documentation sources as a whole.
- `../src/` — module/`-doc` sources that generate the API reference and link into these guides.
- `../include/public_key.hrl` — the record and OID definitions the records guide documents.
- `../../../make/ex_doc.exs` — shared ExDoc driver that auto-includes `guides/*.md`.
