# ssl application documentation (user-facing, ExDoc)

## Intent
Source of the published user documentation of the `ssl` application (HTTPS-style HTML + man pages are generated from here).
Holds only *extra pages* (hand-written markdown) plus the ExDoc build config; the per-module API reference is authored elsewhere (see below).
Built with ExDoc via `make/docs.mk`; output is installed under `$(RELEASE_PATH)/lib/ssl-<VSN>/doc/html` and `$(RELEASE_PATH)/man/`.

## Doc pipeline (what consumes what)
- `Makefile` includes `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(SSL_VSN)`), sets `APPLICATION=ssl`, then includes `$(ERL_TOP)/make/doc.mk` — all real logic lives in `make/doc.mk`.
- Targets: `docs` (default: `html` + `man`), `html`, `man`/`man1..man7`, `chunks` (no-op here, `CHUNK_FILES` is unset), `clean`/`clean_docs` (rm `html/`, `man*/`), `release_docs_spec` (install into the release).
- `html` runs `make/ex_doc_wrapper` → `escript $(EX_DOC) ssl $(DOC_VSN) ../ebin -o html -c $(ERL_TOP)/make/ex_doc.exs`; ExDoc reads the EEP-48 `Doc` chunks of the *compiled beams in `../ebin`* for module docs, so the app must be compiled first (`HTML_DEPS` includes `../ebin/*.beam`).
- Module docs (`-moduledoc`/`-doc`) live in `../src/*.erl` — there is no module doc source in this directory.
- ExDoc extras are merged in `make/ex_doc.exs`: local `docs.exs` `extras` first, then `Path.wildcard("*.md")` + `Path.wildcard("{guides,references,internal_docs}/*.md")` — so `notes.md` and `ssl_app.md` are picked up automatically, and only `guides/` is scanned as a directory here.
- man pages: `man3/*.3` are generated from the `../src/*.erl` sources by `make/markdown_to_man.escript` (→ `man_docs:module_to_manpage`), filtered by `grep -L "moduledoc false."`; `man6/ssl.6` is generated from `ssl_app.md` (the `*_app.md` wildcard).
- Guides are ordered in the sidebar by the `extras:` list in `docs.exs`; `notes.md` is appended by the wildcard and lands outside the default groups.

## API Surface
- `docs.exs` — local ExDoc config; currently only `extras:` (order: `guides/ssl_protocol.md`, `guides/using_ssl.md`, `guides/ssl_distribution.md`, `guides/ssl_hardening.md`, `guides/standards_compliance.md`, `ssl_app.md`).
- `Makefile` — thin wrapper described above; no `DOC_TARGETS`/`SUB_DIRECTORIES` overrides.
- `ssl_app.md` (169 lines) — the application page (also man6 `ssl.6`): description, dependencies (`Public_Key`, `Asn1`, `Crypto`), the complete list of `ssl` application environment parameters (`protocol_version`, session cache/ticket and PEM-cache options, `alert_timeout`, `internal_active_n`, …), logger/event-handler behaviour, "See Also".
- `guides/ssl_protocol.md` (138 lines) — TLS/DTLS protocol overview: purpose, Erlang support, security/data-privacy/digital-certificate concepts, peer authentication, sessions prior to TLS-1.3 and TLS-1.3 session tickets.
- `guides/using_ssl.md` (940 lines) — the examples/tutorial guide: basic client and connect/listen snippets, TLS-only upgrade, custom cipher suites, TLS-1.2 signature algorithms vs TLS-1.3 schemes, engine-stored keys, NSS keylog, session reuse pre-1.3, TLS-1.3 tickets/resumption, early data + anti-replay, DTLS.
- `guides/ssl_distribution.md` (299 lines) — Erlang distribution over TLS: boot script contents, `-proto_dist` selection (`inet_tls`/`inet6_tls`), TLS options for `net_kernel`, a `verify_fun` example, IPv6 distribution.
- `guides/ssl_hardening.md` (327 lines) — recommended hardening settings: algorithm selection (cipher suites, signature algorithms, post-quantum), privacy/integrity (session key renewal, resumption + client certs, key-exchange groups, PSK, early data), authenticity (certificates/keys, `verify_fun`, certificate revocation).
- `guides/standards_compliance.md` (492 lines, ~135 KB — mostly tables) — conformance status per protocol: common (pre- and post-TLS-1.3), SSL 2.0/3.0, TLS 1.0–1.3, DTLS 1.0/1.2/1.3, PQC (ML-KEM key exchange, signature algorithms), "Not Yet Implemented".
- `notes.md` (5906 lines, "SSL Release Notes") — per-release changelog from `SSL 4.0` to the current `11.7.x`, top section = newest release; published as an extra page (its undefined-reference warnings are suppressed by `skip_undefined_reference_warnings_on: ["notes.md"]` in `make/ex_doc.exs`).
- `src/ssl.fig` — legacy xfig diagram, not referenced by any file or make rule; `src/` has no other content and contributes nothing to the build (the `src/*.md` and `references/*.md` globs in `make/doc.mk` match nothing here).

## Constraints
- Write only markdown/GFM (earmark_parser flavor); inline HTML is not allowed as it breaks shell docs and man pages.
- Intra-doc links (`m:ssl`, `t:ssl:cert_key_conf/0`, `ssl:cipher_suites/2`, `[text](standards_compliance.md)`) must resolve against the module docs in `../src` or a sibling file; ExDoc runs with `EX_DOC_WARNINGS_AS_ERRORS=default` (errors when all apps are built), so a broken link fails the build.
- Text must match the real API/behaviour implemented in `../src`; the env parameters in `ssl_app.md` must stay in sync with `../src/ssl.app.src` and the defaults in `../src/ssl_config.erl`.
- Every markdown file needs the `<!-- %CopyrightBegin% ... -->` Apache-2.0 header (enforced by `scripts/license-header.es` for `.md` files longer than 20 lines).
- Code blocks are rendered/highlighted only — no CI step compiles or executes the guide examples.
- Do not add images here: assets belong in a `doc/assets/` directory mapped by `make/ex_doc.exs` (none exists for `ssl`).

## Notes for Agents
- Build this directory with `make docs` (or `make docs DOC_TARGETS=html`); `html/` and `man*/` are generated and git-ignored (root `.gitignore`: `/lib/*/doc/html`, `*.3`, `*.6`).
- Requires a compiled app (`../ebin/*.beam`) and `ex_doc` on `PATH` (bootstrap with `$ERL_TOP/otp_build download_ex_doc`; prompt logic in `make/check_ex_doc`); `ERL_TOP` and a released `bin/` in `PATH` are also expected.
- `notes.md` is release-process material: new sections are added at the top under `# SSL Release Notes` (as done in "Prepare release" commits) — do not edit it as part of code changes.
- `man` is part of the default `DOC_TARGETS` here even though `HOWTO/INSTALL.md` only advertises `html`/`chunks`.
- Text in `guides/` duplicates conceptual material from `../src/ssl.erl`'s moduledoc and from `../internal_doc/ssl_notes.md`; keep the developer-internal version in `../internal_doc/` only.

## Dependencies
- ExDoc (version tracked in `make/ex_doc_vsn`) plus an installed OTP (`bin/` in `PATH`) to run the wrapper.
- `make/markdown_to_man.escript` and `make/fixup_doc_links.sh` (release link rewriting) from the shared build system.

## Routing Table
- `guides/` → the five published user guides (protocol overview, examples, distribution, hardening, standards compliance)
- `src/` → legacy container holding only the unused `ssl.fig` diagram
- `../src/` → module docs (`-moduledoc`/`-doc`) and `ssl.app.src` env definitions — source of truth for everything documented here (sibling — read-only)
- `../internal_doc/` → developer notes (`ssl_notes.md`), *not* published by this build (sibling — read-only for this node)
- `../examples/` → `certs/` key/certificate files that the guide examples assume in the working directory (sibling — read-only)

## See Also
- `../CONTEXT.md` — ssl application as a whole; repo-root `CONTEXT.md`, `../make/CONTEXT.md` (`doc.mk`, `ex_doc.exs`, `markdown_to_man.escript`), `HOWTO/DOCUMENTATION.md` (style rules for extra pages).
