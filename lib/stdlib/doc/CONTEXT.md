# stdlib/doc — Documentation Sources for STDLIB

## Intent
The documentation source tree for the `stdlib` OTP application, fed to ExDoc to produce the published HTML (and `markdown_to_man.escript` for man pages).
Most stdlib modules document themselves from `-moduledoc`/`-doc` attributes in `../src/*.erl`; this directory holds the ExDoc configuration plus the supplementary markdown that has no home in a module — the app overview, user's guides, reference pages, sample scripts and the release notes.
Builds in place; output goes to `../doc/html` in the build tree, never committed here.

## API Surface
- `Makefile` — includes `$(ERL_TOP)/make/target.mk` and `otp.mk`, then `../vsn.mk` (`VSN=$(STDLIB_VSN)`), sets `APPLICATION=stdlib`, and includes `$(ERL_TOP)/make/doc.mk` (the real driver, shared by all apps). This directory only supplies metadata; the ExDoc invocation lives in `doc.mk`.
- `docs.exs` — ExDoc config (Elixir keyword list). Currently sets exactly two keys:
  - `groups_for_modules:` — the module grouping shown in the sidebar. Groups in order: BASIC DATA TYPES, HIGH-LEVEL DATA STRUCTURES, STRINGS, PROCESSES, IO, TERM STORAGE, CODE, COMPRESSION, SHELL. Members are atoms (`:lists`, `:gen_server`, …). `:erl_id_trans` is listed under CODE but its source lives in `../examples/erl_id_trans.erl`, not `../src/`.
  - `extras:` — ordered list of markdown pages; ordering here determines sidebar order. Entries: the 7 `guides/*.md`, `references/assert_hrl.md`, then `stdlib_app.md`.
- `stdlib_app.md` — the STDLIB application overview page (man page `stdlib_app`/section 6): description + the list of `shell_*`/`restricted_shell` application configuration parameters, with anchors (e.g. `#shell_history_length`) referenced from module docs.
- `notes.md` — the STDLIB release-notes source (`# STDLIB Release Notes`), ~10k lines covering `## STDLIB 8.1` down to `1.13.1`. It is the app's change log; ExDoc suppresses undefined-reference warnings on it (`skip_undefined_reference_warnings_on: ["notes.md"]`).
- `assets/` — example scripts referenced by the guides: `pshell.es` (custom process-inspection shell, from `guides/custom_shell.md`) and `tic-tac-toe.es` (terminal game, from `guides/terminal_interface.md`). Copied verbatim into the HTML output.
- See the routing table for `src/`, `guides/`, `references/`.

## Constraints
- **Module docs usually do NOT live here.** Only 6 modules have hand-written markdown in `src/`. Everything else (all `ets`, `lists`, `gen_server`, `io`, … docs) is authored in `-moduledoc`/`-doc` attributes in `../src/*.erl` — edit those, not this directory.
- `doc/src/<mod>.md` overrides the module's attribute docs entirely; a module with a file here must not also carry a `-moduledoc`.
- A module absent from `groups_for_modules` still renders, but appears in a trailing ungrouped section rather than inside a named group — keep the list in sync when adding/removing documented modules.
- `docs.exs` extras are merged with a wildcard of `*.md`, `guides/*.md`, `references/*.md` in `make/ex_doc.exs`; any markdown dropped in those dirs is published even if not listed, but only listed files get a controlled position. Add new guides to `extras:` to place them predictably.
- Man pages are generated from `src/*.md` and `references/*.md` by `$(ERL_TOP)/make/doc.mk`; the top-level `*.md` and `guides/*.md` are HTML-only.
- Do not edit generated HTML; it is build output of `make docs`/`make html` and is not tracked.
- `notes.md` is release-notes input — append new entries through the normal release process, don't rewrite history in it.

## Notes for Agents
- The actual ExDoc behaviour (extras globbing, `groups_for_extras`, deps/links to other apps' docs, `groups_for_docs` for types/callbacks, warning-as-error) is defined in the repo-root `make/ex_doc.exs`; `docs.exs` here is only the per-app override merged into it.
- `guides/*.md` and `references/*.md` start with an HTML-comment copyright block; module doc sources in `src/` have no `# Title` — their first paragraph is the module summary.
- Documentation only; nothing here is compiled by the Erlang compiler or covered by Common Test.

## Routing Table
- `src/` → per-module markdown doc sources for the 6 modules documented outside their `.erl` (`ms_transform.md`, `peer.md`, `qlc.md`, `re.md`, `shell.md`, `sofs.md`); also feed the section-3 man pages
- `guides/` → user's guides published under "User's Guides" (introduction, io_protocol, custom_shell, terminal_interface, unicode_usage, uri_string_usage, re_incompat)
- `references/` → reference pages grouped as "References" (`assert_hrl.md` documents `include/assert.hrl`)
- `assets/` → downloadable example escripts linked from the guides (`pshell.es`, `tic-tac-toe.es`)

## See Also
- `../src/` — where the majority of module documentation is authored (`-moduledoc`/`-doc` attributes); this directory documents only what does not fit there.
- `../include/assert.hrl` — documented by `references/assert_hrl.md`.
- `../../make/doc.mk` and `../../make/ex_doc.exs` — the shared ExDoc/man-page build driver consumed by this directory's `Makefile`.
- `../../system/` — builds the combined OTP documentation set (`index` app) that links into each app's HTML.
