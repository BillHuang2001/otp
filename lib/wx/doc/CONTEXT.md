# wx — Documentation Build

## Intent
Sources and build glue for the published `wx` application documentation (release notes, the user guide, and the HTML/man output built from module docs).
Flat leaf directory: it contains no sub-sources other than `guides/`; all Erlang documentation text lives in the `-moduledoc`/`-doc` attributes of `../src/`.
Built as a subdirectory of `../Makefile` (`SUBDIRS += examples doc`, normal builds only — never under `BOOTSTRAP`).

## API Surface
- `Makefile` — includes `../config.mk`, `../vsn.mk` (`VSN=$(WX_VSN)`), sets `APPLICATION=wx`, then includes `$(ERL_TOP)/make/doc.mk`; provides `docs`/`release_docs`/`clean`.
- `docs.exs` — local ExDoc config; only sets `extras: ["guides/chapter.md"]` (its order fixes the extras listing).
- `guides/chapter.md` — the user guide "wx the erlang binding of wxWidgets" (introduction, multiple processes/memory handling, event handling, acknowledgments).
- `notes.md` — the application **release notes** (per-version "Fixed Bugs and Malfunctions" / "Improvements and New Features" with OTP ids and PR links).
- `overview.edoc` — legacy edoc overview (`@title`/`@author`/`@doc`, `{@section ...}` markers); **dead** (see Notes for Agents).
- Generated, git-ignored output: `html/` (ExDoc HTML; ignored repo-wide by `/lib/*/doc/html`) and untracked `man/` sources.

## Build Flow (how these files are consumed)
- `make docs` / `make release_docs` in `../` reach `make/doc.mk`, which offers `html` (ExDoc) and `man` (markdown_to_man.escript) targets.
- Extras are assembled in `make/ex_doc.exs` as local `extras` ++ `*.md` ++ `{guides,references,internal_docs}/*.md`, so `notes.md` and `guides/chapter.md` are **both** rendered as extras regardless of `docs.exs`.
- HTML is produced by `$(ERL_TOP)/make/ex_doc_wrapper` over `../ebin` (the module docs come from the compiled `../src/`), and `release_html_spec` installs it to `$(RELEASE_PATH)/lib/wx-$(VSN)/doc/html`.
- Man pages are generated for every `../src/**/*.erl` that is not marked `-moduledoc false` (238 of 243 files, including the generated `../src/gen/` class modules); internal modules such as `wxe_master.erl`/`wxe_server.erl` are excluded.

## Constraints
- Doc *text* for modules belongs in `../src/` (`-moduledoc`/`-doc` attributes), not in this directory; files here are only free-standing prose extras.
- `notes.md` is release-managed — its structure/format is maintained by the release process; do not treat it as free-form documentation or edit it by hand.
- There is no `references/` subtree here, so no `*_cmd.md` man1 pages are emitted for `wx`.
- Do not add per-class doc files for the generated `wx*` modules; they are documented from `../src/gen/*.erl` (produced by `../api_gen/`).

## Notes for Agents
- `overview.edoc` is **dead**: nothing in `make/doc.mk`, `make/ex_doc.exs`, the `wx` Makefile or any other build file references it, and ExDoc only picks up `*.md` extras — it is never built or installed.
- Its content is superseded by `guides/chapter.md`, which is the live port of the same overview text (the older file still carries the `wxErlang`/2009 copyright wording).
- Prefer deleting or ignoring `overview.edoc` rather than updating it.
- The layout mirrors the doc dirs of `../eunit/`, `../edoc/` and `../syntax_tools/` (single `guides/chapter.md` + `docs.exs` extras).

## See Also
- `../CONTEXT.md` — `wx` app overview, generated-code constraints, `api_gen/` code-generation pipeline.
- `../../make/doc.mk`, `../../make/ex_doc.exs` — the shared ExDoc/man build driver and global extras rules.
- `../../system/` — the `docs.exs`/index-level documentation build that consumes the per-app HTML.
