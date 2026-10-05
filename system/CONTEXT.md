# Erlang/OTP System Documentation (`system/`)

## Intent
Holds the **system-level documentation** of Erlang/OTP: the language- and platform-oriented User's Guides (reference manual, OTP design/system principles, efficiency guide, interoperability tutorial, getting-started, installation, embedded, SBOM, VEX) plus the generator for the **top-level documentation index** (`$(ERL_TOP)/doc/index.html`) that ties all per-application doc trees together.
Purely documentation: no OTP application, no `src/`, no `ebin/`, nothing compiled here. The only subdirectory is `doc/`.

## API Surface
- `doc/Makefile` — pseudo-application (`APPLICATION=Erlang System Documentation`) that includes `$(ERL_TOP)/make/doc.mk`; drives the guide build and the `general_info`/`installation_guide`/`assets` generation steps.
- `doc/docs.exs` — ex_doc config for this tree: the explicit `extras:` publication list, `main: "readme"`, `api_reference: false`, and `groups_for_extras` computed at build time from `doc/guides`.
- `doc/guides` — extensionless, the single source of truth for the guide/sidebar list (12 `dirname:Title` lines). Consumed three ways: `docs.exs` group regexes, `GUIDES` in `Makefile`, and `SYSTEM` in `doc/top/Makefile`.
- `doc/guides.license` — REUSE sidecar granting `guides` its Apache-2.0 header.
- `doc/README.md` — landing page of the system docs; also the ex_doc `main` page.
- `doc/top/` — a **separate** ex_doc app (`APPLICATION=Erlang/OTP`, `EPUB=false`) that builds `$(ERL_TOP)/doc/index.html`, per-app/per-guide redirect stubs, and `man_index.md` (via `top/src/otp_man_index.erl`).

## Build & Output
- Invoked from the repository-root `Makefile.in` `docs` / `release_docs` targets, which build in the order `lib` → `erts` → `system/doc` → `system/doc/top`.
- Shared machinery: `make/doc.mk` (docs/html/man targets), `make/ex_doc.exs` (global ex_doc config, merges each directory's `docs.exs`), `make/ex_doc_wrapper` (locates/downloads ex_doc, sets `EX_DOC_VERSION`, warning policy), `make/ex_doc_vsn` (pinned version, currently `v0.40.4`), `make/check_ex_doc`, `make/get_ex_doc_formats_no_epub.sh`, `make/fixup_doc_links.sh` (rewrites `system/doc/html` → `doc/system` on release install).
- Outputs: `system/doc/html/` (installed to `$(RELEASE_PATH)/doc/system`) and the top-level `doc/` tree produced by `doc/top/`.
- Requires a preceding full `lib` + `erts` build: autolinks resolve against EEP-48 chunks in `*.beam`, and the deprecation pages scan `lib/*/ebin`.

## Constraints
- Never hand-edit generated content: `installation_guide/INSTALL*.md` + `OTP-PATCH-APPLY.md` (copied from root `HOWTO/` with `sed` substitutions), `general_info/{deprecations,scheduled_for_removal,removed}.md`, `assets/*` (flattened copies + `versions.erl`), and everything generated under `doc/top/`.
- Every published page must be listed in `doc/docs.exs` `extras:` — guide subdirectories are not globbed, so an unlisted page is silently dropped.
- Adding a guide requires a `guides` line **and** `docs.exs` entries; `doc/top` picks the line up automatically.
- Page identity for cross-tree autolinks is the **basename** (`e:system:<basename>.md`, `e:general_info:*`), so basenames must stay unique across `system/doc`.

## Known Issues
- `doc/guides` line `secure_coding:Secure Coding Guidelines` refers to no directory; the page is `doc/design_principles/secure_coding.md`, already grouped by the `design_principles` regex (first match wins).
- `doc/README.md` links to `vex/vulnerabilities.md`, which does not exist (actual: `vulnerabilities/vulnerabilities.md`); plain relative `.md` links are not validated, so it fails silently. README also omits `sbom`, `secure_coding` and `general_info` from its guide list.
- `doc/assets/versions.erl` is a build-time copy of `lib/runtime_tools/src/versions.erl` that is not git-ignored (only `*.png|*.gif|*.svg` are), so it appears untracked after `make docs`.

## Notes for Agents
- `CONTEXT.md` files inside `doc/<guide>/` are never published; a `CONTEXT.md` placed directly in `doc/` **is** auto-published into the ex_doc build (because `make/ex_doc.exs` globs `*.md`). This file (`system/CONTEXT.md`) is outside that cwd and is unaffected.
- `make clean`/`clean_docs` here removes only `$(EXTRA_FILES)` (currently unset) — generated `html/`, `assets/`, and the generated `.md` files survive.
- Content changes can only be verified with a real `make docs` (needs built `lib`+`erts` and an `ex_doc` binary).

## Dependencies
- `ex_doc` (version pinned in `make/ex_doc_vsn`; fetched interactively via `otp_build download_ex_doc` by `make/check_ex_doc`).
- Erlang/OTP itself (escripts: `lib/stdlib/scripts/update_deprecations`, `make/markdown_to_man.escript`, `doc/top/src/otp_man_index.erl`).
- Network access at *render* time for mermaid diagrams (CDN script injected by `make/ex_doc.exs`).

## Test Strategy
- The build is the primary check: `EX_DOC_WARNINGS_AS_ERRORS` defaults to `true`, so bad `e:`/`m:` autolinks and missing `assets/<file>` references fail the build.
- `scripts/otp_check_html_links.exs` (via `scripts/otp_build_check`) validates links in the built HTML — built output only, so anchors and relative `.md` links inside Markdown stay unchecked.
- Doctests couple Markdown in this tree to OTP test suites, e.g. `reference_manual/ref_man_native_records.md` is exercised from `lib/stdlib/test/records_SUITE.erl`.
- There is no test suite under `system/` itself.

## Routing Table
- `doc/` → all system documentation sources and the ex_doc build; see `doc/CONTEXT.md` for the pipeline and the per-guide routing table.
- Sibling references (read-only; escalate writes to the repository root):
  - `../make/` → shared doc build machinery (`doc.mk`, `ex_doc.exs`, `ex_doc_wrapper`, `fixup_doc_links.sh`)
  - `../HOWTO/` → authoring source for `doc/installation_guide/`
  - `../lib/stdlib/scripts/update_deprecations` → generator for `doc/general_info/` pages
  - `../lib/*/doc/`, `../erts/doc/` → per-application doc trees that link into `system` via `e:system:*`
  - `../scripts/` → `otp_check_html_links.exs`, license-header scanner
  - `../.github/` → SBOM/OpenVEX generation consuming `sbom.md` / `vulnerabilities.md`
