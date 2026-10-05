# snmp — Documentation Tree

## Intent
Source of the `snmp` application's user guide and reference manual.
ExDoc renders the markdown here plus the module docs from `../src/` into HTML, and `make` converts the same markdown into man pages.
Part of the release-wide doc build driven from `../../system/doc/`.

## API Surface
- `docs.exs` — ExDoc local config. Only key is `extras`: an ordered list of guide/reference markdown paths (order determines the order they appear in the generated docs).
- `guides/*.md` — 18 user-guide chapters, ordered in `docs.exs`: `snmp_intro`, `snmp_agent_funct_descr`, `snmp_manager_funct_descr`, `snmp_mib_compiler`, `snmp_config`, `snmp_agent_config_files`, `snmp_manager_config_files`, `snmp_impl_example_agent`, `snmp_impl_example_manager`, `snmp_instr_functions`, `snmp_def_instr_functions`, `snmp_agent_netif`, `snmp_manager_netif`, `snmp_audit_trail_log`, `snmp_advanced_agent`, `snmp_app_a`, `snmp_app_b`, `snmp_app_c`.
- `references/snmpc_cmd.md` — man-page style page for the `snmpc` MIB-compiler frontend.
- `snmp_app.md` — the application reference chapter (description, configuration parameters, MIB list); maps to `snmp_app(6)`.
- `notes.md` — SNMP release notes (one `## SNMP <vsn>` section per release, newest first); not listed in `docs.exs` but auto-included as an extra.
- `Makefile` — docs build: includes `../vsn.mk` (`VSN=$(SNMP_VSN)`) and `$(ERL_TOP)/make/doc.mk`; targets `docs`/`html`/`chunks`/`man` and `release_*`.
- `assets/*.gif` — images embedded by the guides via relative `assets/<name>.gif` links.
- `src/*.fig`, `src/*.gif` — editable figure sources/diagrams for the images (not referenced from the markdown).
- `specs/` — build-output dir, git-ignored except `.gitignore` (ignores `specs_*.xml`).

## Constraints
- The generated docs come from three markdown sources: root `*.md` (e.g. `notes.md`), `guides/*.md`, and `references/*.md`; `make/ex_doc.exs` glob-adds all of these as ExDoc extras.
- Only the paths listed in `docs.exs` `extras` get a deterministic position/order; other discovered markdown files still appear (appended), so a new guide should be added to `docs.exs` to keep the reading order right.
- Module reference pages (`snmp`, `snmpa`, `snmpm`, `snmpc`, …) are generated from `-moduledoc`/`-doc` in `../src/**.erl` — never duplicate API reference text here.
- Every image (`assets/*.gif`, `src/*.fig`, `src/*.gif`) has a matching `<file>.license` sidecar (SPDX `Apache-2.0`) — a new binary asset must ship one or the license scanner fails.
- Markdown files carry a `<!-- %CopyrightBegin% ... SPX-License-Identifier: Apache-2.0 ... %CopyrightEnd% -->` header; `Makefile`/`docs.exs` use the `#`-comment form.
- Never hand-edit anything under `specs/` or the generated `html/`, `man*/` output.

## Design Decisions
- Guides are plain markdown linked by relative paths (e.g. cross-chapter links like `snmp_agent_funct_descr.md#image-1`), so filenames are effectively stable public anchors.
- Man pages are derived from the same markdown via `$(ERL_TOP)/make/markdown_to_man.escript`; `make/doc.mk` maps `references/*_cmd.md`→man1, `src|references/*.md` and `*.erl`→man3, `*_app.md`→man6, `../mibs/*.mib`→man7.

## Notes for Agents
- The ExDoc build is invoked as `ex_doc <app> <vsn> <ebin> -c $(ERL_TOP)/make/ex_doc.exs` from `make/doc.mk`; it needs the app's `../ebin/*.beam` to exist, so build the app before building docs.
- `notes.md` mirrors release information; add a new section only when a release is prepared (release commits touch it).

## Dependencies
- ExDoc (`make/check_ex_doc`, `make/ex_doc_wrapper`) for HTML/chunks; a system `escript` for man pages; `make/markdown_to_man.escript`.

## Test Strategy
- There is no test suite in this directory; correctness is checked by `make docs` (ExDoc warnings are errors by default via `EX_DOC_WARNINGS_AS_ERRORS=default`) and by the copyright/license scanners (`scripts/`).

## Routing Table
| Path | Description |
| --- | --- |
| `guides/` | 18 user-guide chapters (markdown), ordered by `docs.exs` |
| `references/` | Reference/man-page markdown (`snmpc_cmd.md`) |
| `assets/` | Rendered `.gif` images used by the guides, each with a `.license` sidecar |
| `src/` | Editable `.fig`/`.gif` figure sources, each with a `.license` sidecar |
| `specs/` | Generated spec output (git-ignored, empty until built) |
| `snmp_app.md` | Application reference chapter (→ `snmp_app(6)`) |
| `notes.md` | SNMP release notes |
| `../src/` | Erlang sources whose `-moduledoc` supplies the module reference pages (sibling — read-only) |
| `../vsn.mk` | `SNMP_VSN` consumed by `Makefile` (sibling — read-only) |
| `../../make/doc.mk` | Shared docs-build makefile that defines the html/man targets (read-only) |
