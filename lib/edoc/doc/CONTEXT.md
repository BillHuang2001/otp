# edoc — Documentation Sources

## Intent
Documentation sources for the `edoc` OTP application (the Erlang documentation generator).
Contains the hand-written guides/references (markdown), the ExDoc config, and helper inputs used to build the edoc documentation and its EEP-48 doc chunks.
The published HTML is generated from the `src/` module doc attributes (`%% @doc ...`) plus the markdown files here.

## API Surface
- `docs.exs` — ExDoc local config read by `make/ex_doc.exs`; only sets `extras: ["guides/chapter.md", "references/edoc_cmd.md"]` (the explicit order fixes how these are listed in the docs).
- `Makefile` — includes `../src/files.mk` (defines `ERL_FILES`, `YRL_FILES`) and `$(ERL_TOP)/make/doc.mk`; adds `assets/` copies and the `chunks` target on top of the standard doc build.
- `notes.md` — release notes for the app (auto-included as a doc extra by the `*.md` wildcard).
- `guides/chapter.md` — "Welcome to EDoc", the EDoc User Manual.
- `guides/doc_storage.md` — "EEP-48: Documentation storage and format" / Erlang Documentation Format guide.
- `references/edoc_cmd.md` — reference for the `edoc` command-line tool (`-app`/`-files`, `-chunks`, per `edoc:application/2` / `edoc:files/2`).
- `edoc_doclet.hrl` — symlink to `../include/edoc_doclet.hrl` (doclet record defs).
- `edoc.dtd` — symlink to `../priv/edoc.dtd` (legacy EDoc XML DTD).

## How the docs are built
- `make docs` / `make html` in this dir (driven by `lib/edoc/Makefile`, which lists `doc` in `SUB_DIRECTORIES`) runs `$(ERL_TOP)/make/ex_doc_wrapper` with `-c $(ERL_TOP)/make/ex_doc.exs`.
- `make/ex_doc.exs` merges the local `docs.exs` `extras` with the auto-discovered markdown (`*.md`, `guides/*.md`, `references/*.md`), so all four markdown files above become doc pages without being listed individually.
- `html/` is the ExDoc output dir (`HTMLDIR`); `html/` receives generated HTML and is git-ignored.
- `assets/edoc_doclet.hrl` and `assets/edoc.dtd` are `cp`-ed from `../include/` and `../priv/` so that doc-comment links (`href="assets/edoc.dtd"`, `href="assets/edoc_doclet.hrl"` in `src/edoc.erl` / `src/edoc_doclet.erl`) resolve in the built docs; `assets/*` is a prerequisite of `$(HTMLDIR)/index.html`.
- `make chunks` (also in `DEFAULT_DOC_TARGETS` via `make/doc.mk`) generates EEP-48 `.chunk` files into `chunks/` using `edoc:files([...],[{doclet,edoc_doclet_chunks},{layout,edoc_layout_chunks},{preprocess,true},{includes,["../include"]}])`.
  - `NO_PARSE_SOURCES = $(filter-out edoc_parser.erl, $(ERL_FILES))` — `edoc_parser.erl` is excluded from chunks (it is generated from `edoc_parser.yrl`, whose `YRL_FILES` are chunked instead).
  - The `chunks`/`$(CHUNKSDIR)/%.chunk` targets here override the generic ones in `make/doc.mk` for this app.

## Sources vs. build outputs
- Source (tracked): `docs.exs`, `Makefile`, `notes.md`, `guides/`, `references/`, the two symlinks.
- Build output (git-ignored, only `.gitignore` tracked): `assets/` (receives `.hrl`/`.dtd` copies), `chunks/` (`*.chunk`), and `html/` (`HTMLDIR`).

## Constraints
- Never edit the generated `assets/` or `chunks/` contents; they are produced by `make`.
- The two top-level `edoc_doclet.hrl` / `edoc.dtd` entries are symlinks — edit the real files in `../include/` and `../priv/`, not here.
- Guide/reference markdown is consumed both by the ExDoc build and, for module-doc cross-refs, by links like `` `e:edoc:doc_storage.md` `` referenced from `guides/chapter.md`.

## Notes for Agents
- `docs.exs` only overrides `extras`; other ExDoc settings come from the shared `$(ERL_TOP)/make/ex_doc.exs`, so changing extras/ordering is the only reason to touch this file.
- `lib/edoc/Makefile` still carries an EDoc/HTML legacy path (`EXTRA_FILES = edoc-info overview.edoc stylesheet.css`, `HTML_FILES = doc/*.html`, and an `edocs` target running `edoc_run application`) that predates the ExDoc build and is not part of the normal doc flow.
- The `## CopyrightBegin %>`-style comment banners at the top of each markdown file are the OTP license header format, not ExDoc front-matter.

## Routing Table
- `guides/` → User-manual / format guides (`chapter.md`, `doc_storage.md`); each `*.md` becomes a doc page.
- `references/` → Command/API references (`edoc_cmd.md`); each `*.md` becomes a doc page.
- `docs.exs` → ExDoc `extras` ordering config for this app.
- `notes.md` → Release notes (auto-included doc extra).
- `edoc_doclet.hrl` → symlink to `../include/edoc_doclet.hrl` (also copied into `assets/`).
- `edoc.dtd` → symlink to `../priv/edoc.dtd` (also copied into `assets/`).
- `assets/` → git-ignored build output (copied `.hrl`/`.dtd` for doc links).
- `chunks/` → git-ignored build output (generated EEP-48 `*.chunk` files).

## See Also
- `../CONTEXT.md` — the edoc application (build/`Makefile`, `src/files.mk`, `include/`, `priv/`).
- `../src/files.mk` — defines `ERL_FILES`/`YRL_FILES` consumed by this `Makefile`.
- `../../../make/doc.mk`, `../../../make/ex_doc.exs` — shared ExDoc/chunks build infrastructure.
