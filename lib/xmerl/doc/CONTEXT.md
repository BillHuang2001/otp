# xmerl/doc — `xmerl` documentation (ExDoc)

## Intent
Publishes the user-facing documentation of the `xmerl` application through the shared OTP ExDoc build.
It holds only hand-written markdown extras (guides + release notes), the tutorial's sample XML inputs/expected outputs, and a few standalone example programs.
The API reference is **not** here: there is no `doc/src/`; module pages are generated from the `-moduledoc`/`-doc` attributes in `../src/*.erl`.

## API Surface
- `Makefile` — documentation entry point: includes `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(XMERL_VSN)`), then sets `APPLICATION=xmerl`, `EPUB=false` and includes `$(ERL_TOP)/make/doc.mk` (targets: `docs`, `html`, `chunks`, `man`, `release_html_spec`, `clean_docs`).
- `docs.exs` — the app-local ExDoc config: `skip_code_autolink_to: ["t:xmerl_scanner/0","t:xsd_state/0"]` (silences undefined-reference warnings for those two type names, which are referenced from module docs but not generated) and `extras: ["guides/xmerl_ug.md"]` (this list only fixes **order**; `../../make/ex_doc.exs` additionally wildcard-adds `*.md` and `{guides,references,internal_docs}/*.md`).
- `notes.md` (~1450 lines) — "Xmerl Release Notes", one `## Xmerl <vsn>` section per release newest-first with `Own Id`/`Aux Id` bullets and PR link definitions; appended by the release process, not generated from an in-repo source.
- `guides/xmerl_ug.md` — the Xmerl User's Guide (Features, Overview, "Xmerl User Interface Data Structure", Getting Started, and three worked examples: extracting data from XML, creating XML from arbitrary data, transforming XML to HTML); it is the extra listed first and therefore drives page order.
- `guides/xmerl_examples.md` — the "Customization functions" tutorial; referenced from `../src/xmerl_scan.erl` (many call sites) and `../src/xmerl_eventp.erl` as `` `e:xmerl:xmerl_examples.html` ``.
- `guides/xmerl_xs_examples.md` — the "XSLT-like transformations" tutorial; referenced from `../src/xmerl_xs.erl` and from the User's Guide.
- `assets/` — tutorial inputs (`motorcycles.txt`, `motorcycles2.txt`, `new_motorcycles.txt`, `new_motorcycles2.txt`, `motorcycles_dtd.txt`, `people.txt`, `people2.txt`), expected outputs (`result_export.html`, `result_xs.html`), `stylesheet.css`, and `motorcycles2html.erl` (the example program printed in the guide); the `.txt` suffix is deliberate so the files are not parsed/validated as XML by tooling, while the guide text calls them `motorcycles.xml`/`motorcycles.dtd`.
- `assets/` is mounted into the published docs as the `assets` directory by `../../make/ex_doc.exs` (`assets: %{Path.join(cwd, "/assets") => "assets"}`) — this is what makes the `assets/result_*.html` links inside `xmerl_ug.md` resolve.
- `examples/` — standalone sample programs (`xmerl_test.erl`, `xserl_test.erl`, `test_html.erl`, `mkdocs.erl`, `sdocbook2xhtml.erl`) plus `examples/xml/*.xml` and `testdtd.dtd`, built by the legacy `examples/Makefile` (`erl -run mkdocs run <xml> <html>` renders the XML files to HTML with `xmerl` on the path).
- No `xmerl_app.md` exists here (unlike most apps), so the `index.html` landing page comes from ExDoc's app metadata in `../src/xmerl.app.src` (`{description,"XML parser"}`), not from a local markdown file; other apps link to it as `` `e:xmerl:index.html` ``.

## Constraints
- Do not create a `doc/src/` tree — API pages are built from `../src/*.erl` moduledocs, and the legacy edoc doc target in `../src/Makefile` (`DOCDIR`, `EDOC_MODULES`, `$(DOCDIR)/%.html` rule) is inactive (its `docs:` target is commented out).
- `../../make/doc.mk` runs ExDoc with `EX_DOC_WARNINGS_AS_ERRORS=default` ⇒ warnings as errors, so broken `m:`/`t:`/`e:`/relative links or referencing a renamed guide file fails the doc build.
- `doc/examples/xmerl_test.erl` is the **master** copy: `../test/Makefile` copies it to `../test/xmerl_test.erl` (and `make clean` in `../test/` deletes that copy) — edit it here, never the generated copy in `../test/`.
- New release-note entries go at the top of `notes.md` under the current version; do not restructure existing sections.
- Every `.md`, `.erl`, `.exs` and `Makefile` here carries an SPDX `%CopyrightBegin%` header block.
- `assets/*.txt|*.xml` are tutorial inputs, not test fixtures — real CT fixtures live in `../test/*_SUITE_data/`.

## Notes for Agents
- Build the docs with `make docs` (or `make html`) in this directory; it requires `ERL_TOP` to be set, `../ebin` to be populated with the app's beam files (module docs are read from there), and an `ex_doc` download (prompted by `$(ERL_TOP)/make/check_ex_doc`).
- `EPUB=false` makes the build use `$(ERL_TOP)/make/get_ex_doc_formats_no_epub.sh`, i.e. `-f html -f markdown` (`-f html` only for ex_doc < 0.40).
- Build output goes to `html/`, `chunks/`, `man*/` under this directory (`HTMLDIR=$(DOCDIR)/html`); `/lib/*/doc/html` is gitignored — nothing generated here is tracked.
- `examples/Makefile` is not reached from the OTP build (`doc/Makefile` sets no `SUB_DIRECTORIES`, and `make/doc.mk` does not recurse); treat it as a legacy standalone helper.
- `[](){: #label }` occurrences in `xmerl_ug.md` are document anchor definitions (an OTP-wide docs convention), not file includes.

## Test Strategy
- The documentation is not unit-tested; the doc build itself is the check (warnings as errors) — run `make docs`/`make html` here.
- Nothing in `doc/examples/` is part of the test suite except `xmerl_test.erl`, which reaches Common Test only through the copy made by `../test/Makefile`.

## Routing Table
- `guides/` → hand-written guide markdown: `xmerl_ug.md` (User's Guide), `xmerl_examples.md` (Customization functions), `xmerl_xs_examples.md` (XSLT-like transformations)
- `assets/` → tutorial sample inputs/expected outputs and `motorcycles2html.erl`; published as the `assets/` doc directory
- `examples/` → legacy standalone example programs and `examples/xml/` inputs (includes the master `xmerl_test.erl`)
- `docs.exs` → ExDoc local config; `notes.md` → release notes
- See also (read-only): `../src/` (`-moduledoc`/`-doc` = the API reference, `xmerl.app.src` = app metadata), `../test/` (consumes `examples/xmerl_test.erl`), `../../make/doc.mk` + `../../make/ex_doc.exs` (the shared ExDoc driver), `../../../system/` (general documentation toolchain docs)
