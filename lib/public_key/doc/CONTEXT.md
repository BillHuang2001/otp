# public_key — Documentation Sources

## Intent
ExDoc/markdown documentation sources for the `public_key` application (the documentation counterpart of `../src/`).
Sources here are pure markdown — no XML/`doc/src` tree — and are consumed by the release-wide docs build.

## API Surface
- `docs.exs` — local ExDoc config: only an `extras:` list that fixes the ORDER the guide pages appear in the docs (`guides/public_key_records.md`, `guides/using_public_key.md`, `public_key_app.md`).
- `public_key_app.md` — application overview; lists the supported PKIX/PKCS functional areas (RFC 5280, PKCS-1/3/5/8/10, DSS, CMS, PKIXCMP, PKIXCRMF), runtime deps (`crypto`, `asn1`) and notes it is a pure library (no error logger).
- `notes.md` — release notes for the app (~57k), one `## Public_Key <vsn>` section per released version, newest first; hand-maintained, sections added by "Prepare release" commits.
- `guides/using_public_key.md` — user's guide: PEM file examples (DSA/RSA private keys, X509/OTP certificates), encoding to PEM, RSA cryptography, digital signatures, and the detailed certificate hostname-verification process (`verify_hostname`, pinning, custom match ops).
- `guides/public_key_records.md` — reference guide for every public record (`#'RSAPrivateKey'{}`, `#'OTPCertificate'{}`, `#'Extension'{}`, CRL/CSR records …) with field-by-field decoding.
- `Makefile` — includes `../vsn.mk` (`VSN=$(PUBLIC_KEY_VSN)`, `APPLICATION=public_key`) then `$(ERL_TOP)/make/doc.mk`; exposes `html`, `chunks`, `man`, `docs`, `release_*_spec`.

## Constraints
- The module/function API reference is generated from the `-doc`/`-moduledoc` attributes in `../src/*.erl` — do NOT duplicate API reference text here.
- Only edit markdown/hand-written sources in this directory; `html/`, `man1..man7/` and `chunks/` outputs are build artifacts.
- `docs.exs` controls extras ordering only; adding a new top-level `*.md` or a `guides/*.md` file makes it appear automatically (see routing notes) but with default ordering unless listed.

## Design Decisions
- No `doc/src/` or `references/` tree: the app has no per-API manual pages beyond the auto-generated module pages, so all hand-written content lives in the app overview + two guides.
- `notes.md` is not listed in `docs.exs`; the shared ExDoc driver adds every top-level `*.md` as an extra.

## Notes for Agents
- How docs are built: `make docs` (in this dir) runs `$(ERL_TOP)/make/ex_doc_wrapper` with the shared `$(ERL_TOP)/make/ex_doc.exs`, which deep-merges this `docs.exs` over the defaults.
- `make/ex_doc.exs` computes extras as `docs.exs extras ++ *.md ++ {guides,references,internal_docs}/*.md`, so `public_key_app.md` and `notes.md` are always included.
- `public_key_app.md` matches the `*_app.md` pattern → also rendered as the app man6 page.
- Guides are cross-referenced from code: `../src/public_key.erl` links to `public_key_records.md` and `using_public_key.md` anchors, and `../../ssl/src/ssl.erl` links to `e:public_key:public_key_records.md` — renaming files or anchors breaks those links.
- `make/ex_doc.exs` sets `skip_undefined_reference_warnings_on: ["notes.md"]`, so unresolved refs in release notes do not fail the build (`EX_DOC_WARNINGS_AS_ERRORS`).

## Test Strategy
- No tests here; doc correctness is enforced at build time by ExDoc link/reference warnings (`make docs`, `EX_DOC_WARNINGS_AS_ERRORS=default`).
- The `documentation.md` section of `../../system/doc/reference_manual/` is the canonical guide for how these markdown/ExDoc sources must be written.

## Routing Table
- `guides/` → The two hand-written extras: the public-key records reference and the user's guide (with the externally linked anchor contract); see `guides/CONTEXT.md`.

## See Also
- `../src/` — module and `-doc` sources that generate the API reference for this app.
- `../CONTEXT.md` — the `public_key` application as a whole.
- `../../system/` — system-wide docs build and `system/docs.exs`.
- `../../../make/doc.mk`, `../../../make/ex_doc.exs` — shared documentation make/ExDoc infrastructure.
