# Dialyzer — Documentation Sources

## Intent
Documentation sources for the `dialyzer` OTP application (Dialyzer discrepancy analyzer + the `typer` type annotator).
Everything here is input to the ExDoc/man-page build driven by `Makefile` → `$(ERL_TOP)/make/doc.mk`; built output lands in `html/` and `man{1,3}/` (untracked).

## Contents (each file and its role)
- `docs.exs` — ExDoc local config; only sets `extras: ["guides/dialyzer_chapter.md", "references/typer_cmd.md"]`. The list order determines the order those pages are listed in the docs.
- `guides/dialyzer_chapter.md` — the user-facing "Dialyzer" chapter (introduction/scope, PLT usage, command-line & Erlang usage pointers to `m:dialyzer`, "Dialyzer's Model of Analysis" incl. spec-vs-success-typing intersection rules, feedback section). Grouped under "User's Guides".
- `references/typer_cmd.md` — `typer` command-line reference (usage line + option list). Matches the `references/*_cmd.md` pattern, so it is also rendered as man page `typer.1` via `markdown_to_man.escript`, and grouped under "Command Line Tools".
- `notes.md` — Dialyzer Release Notes, `# Dialyzer Release Notes` followed by one `## Dialyzer <vsn>` section per release (newest first, down to 1.0.0) with `### Fixed Bugs and Malfunctions` / `### Improvements and New Features` subsections and `Own Id: OTP-xxxxx Aux Id: [PR-…]` lines. Auto-picked up by the shared ExDoc config (all top-level `*.md` become extras); linked as `[release notes](notes.md)` from module docs.
- `Makefile` — sets `VSN=$(DIALYZER_VSN)` (from `../vsn.mk`), `APPLICATION=dialyzer`, then includes `$(ERL_TOP)/make/doc.mk`.
- `.gitignore` — ignores generated `specs_*.xml`.
- `about.txt` — legacy short blurb ("This is DIALYZER version 2.1.0", original authors). Not a `.md`, so not part of the ExDoc build; unreferenced by any Makefile/script.
- `warnings.txt` — legacy plain-text catalogue of Dialyzer discrepancy/warning categories (type errors, redundant/unreachable code, code relics) with warning texts and descriptions. Unreferenced by the build; the current user-facing description of warnings lives in the `dialyzer` module docs.

## Constraints
- Edit sources here, never build output: `html/`, `man1|3/`, `specs_*.xml` are generated.
- `docs.exs` `extras` order is meaningful (doc listing order); page title = first `#` heading.
- Do not add app metadata here (`VSN`/`APPLICATION` come from `../vsn.mk` and this `Makefile`).
- One Markdown file per page; `.md` files at this directory root and in `guides/`/`references/` are auto-included as extras by `make/ex_doc.exs` even if not listed in `docs.exs`.
- Keep `##` release-note headings in lockstep with `../vsn.mk` (`DIALYZER_VSN`); writing style rules are in `HOWTO/RELEASE-NOTES.md` (only user-observable changes).

## Notes for Agents
- The authoritative Dialyzer command-line/Erlang API reference is NOT here: it lives in the moduledoc of `../src/dialyzer.erl` (man page `dialyzer.3`, page `m:dialyzer`). `guides/dialyzer_chapter.md` deliberately only links to it.
- `typer` has `-moduledoc false.` in `../src/typer.erl`, so `references/typer_cmd.md` is the only typer documentation.
- `about.txt` and `warnings.txt` are stale/orphaned (Dialyzer is at `6.0.3`); only touch them if intentionally reviving or deleting them.
- `../RELEASE_NOTES` is a separate, much older/shorter legacy file; `doc/notes.md` (per-app, rendered) is the maintained release-notes document.
- ExDoc groups for extras are defined globally in `make/ex_doc.exs` (`guides` → "User's Guides", `references/*_cmd.md` → "Command Line Tools", `references` → References); `notes.md` gets the default group.
- Doc build: `make docs` (options via `DOC_TARGETS`), or `release_docs_spec` during a release; `EX_DOC_WARNINGS_AS_ERRORS` makes broken doc links fatal.

## Routing Table
- `guides/dialyzer_chapter.md` → user guide chapter for Dialyzer (prose)
- `references/typer_cmd.md` → typer CLI reference (also man1)
- `notes.md` → Dialyzer release notes
- `docs.exs` / `Makefile` / `.gitignore` → doc build wiring
- `../src/dialyzer.erl` → authoritative Dialyzer API + command-line reference (sibling — read-only, escalate writes to parent)
- `../vsn.mk` → version source for `VSN` and the notes.md headings (sibling — read-only)
