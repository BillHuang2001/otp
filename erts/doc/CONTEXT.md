# erts/doc — ERTS Documentation Sources

## Intent
Hand-written documentation sources for the ERTS application (the runtime system): its user's guides, command-line-tool manual pages, C-level reference manuals (driver/NIF API), plus the auto-surfaced VM-internals design docs. Built by ExDoc into HTML (published under the ERTS application in the OTP doc set) and by `markdown_to_man.escript` into `man1`/`man3`/`man4` pages.
This is the doc source of the `erts` application only. Sibling doc dirs: `../../lib/<app>/doc/`, `../../system/doc/` (system/index docs), and the top-level index at `../../doc/` (`INDEX_DIR`).

## API Surface
Document sets in this directory (all hand-written markdown unless noted):
- `guides/*.md` (13 files) — user's guides for application/system programmers and operators: `introduction`, `communication`, `time_correction`, `match_spec`, `crash_dump`, `alt_dist`, `alt_disco`, `absform`, `tty`, `driver`, `inet_cfg`, `erl_ext_dist`, `erl_dist_protocol`. Grouped in HTML as **"User's Guides"**.
- `references/*.md` (13 files) — reference manuals:
  - Operator command manuals (`*_cmd.md`) → `man1`: `erl`, `erlc`, `escript`, `epmd`, `erlsrv`, `run_erl`, `start`, `start_erl`, `werl`. Grouped in HTML as **"Command Line Tools"**.
  - Application-programmer C API manuals → `man3`: `erl_driver` (driver API), `erl_nif` (NIF API), `driver_entry` (driver callback struct/macros), `erts_alloc` (allocator config). Grouped in HTML as **"References"**.
- `src/erlang_system_info.md`, `src/erlang_port_info.md` — markdown fragments for the `erlang:system_info/1` and `erlang:port_info/2` reference docs. They are pulled in by `erts/preloaded/src/erlang.erl` via `-doc {file,"../../doc/src/<name>.md"}`, and also converted into `man3` pages. Not listed in `docs.exs` extras (included through the module doc).
- `notes.md` (23k+ lines) — the **ERTS release notes**. Included into the HTML docs via the default `Path.wildcard("*.md")` in `make/ex_doc.exs`; undefined-reference warnings are suppressed for it.
- `docs.exs` — ExDoc config for the erts app: `extras` order (guides then references then `../emulator/internal_doc/*.md`), `groups_for_docs` (Predefined datatypes / Types / Functions / Deprecated functions for the `erlang` module), `annotations_for_docs` (adds "auto-imported"/"allowed in guard tests" using `:erl_internal`).
- `Makefile` — declares `APPLICATION=erts`, `INDEX_DIR=../../../doc`, `RELSYSDIR=$(RELEASE_PATH)/erts-$(VSN)`, `EPUB=false`, the `assets` copy rules, and `include $(ERL_TOP)/make/doc.mk`.

### Build contract (from `make/doc.mk`)
- Output dirs (relative to this dir, all generated & git-ignored): `html/` (`HTMLDIR`), `man1/` (`MAN1DIR`), `man3/`, `man4/`.
- `docs` target → `html` (+ `man` when man deps exist) after `check_ex_doc`; `man` builds `$(MAN1_PAGES)/$(MAN3_PAGES)/$(MAN4_PAGES)`.
- HTML: `$(HTMLDIR)/index.html` runs `make/ex_doc_wrapper` with `docs.exs`, `make/ex_doc.exs`, `ERL_FLAGS="-pz $(ERL_TOP)/erts/ebin"` and `--homepage-url "$(INDEX_DIR)/index.html"`; app modules come from `:erlang.pre_loaded()` (see `make/ex_doc.exs`, `app == :erts`).
- `man1/%.1 ← references/%_cmd.md`; `man3/%.3 ← src/%.md` or `references/%.md`; `man4/%.4 ← references/{app,config,...}.md`; all via `make/markdown_to_man.escript`.
- Release install (from `make/otp_release_targets.mk`/`doc.mk`): HTML → `$(RELEASE_PATH)/erts-$(VSN)/doc/html` (`release_html_spec` + `fixup_doc_links.sh`); man pages → `$(RELEASE_PATH)/man/manN`; `RELSYSDIR` is the erts app dir, not `lib/`.
- Invoked from `../../erts/Makefile` (`docs`, `release_docs`) and the top-level `make docs`.

### The `assets` target (generated files, not tracked)
`make` copies (git-ignored by `assets/.gitignore`: `*.png`, `*.svg`, `*.erl`):
- `../emulator/internal_doc/assets/*` (diagrams for the internal design docs),
- `../../lib/kernel/examples/gen_tcp_dist/src/gen_tcp_dist.erl` and `.../erl_uds_dist/src/erl_uds_dist.erl` (referenced by `guides/alt_dist.md`),
- `../example/time_compat.erl` (referenced by `guides/time_correction.md`).

## Constraints
- **Hand-written sources**: everything tracked under `guides/`, `references/`, `src/*.md` (except generated figs), `notes.md`, `docs.exs`, `Makefile`. Edit these; never edit generated build output.
- **Generated / copied (never edit, git-ignored)**: `html/`, `man1/` (`*.1`), `man3/`/`man4/` (`*.3`/`*.4`), and `assets/*.png|*.svg|*.erl`. `src/erl_fix_alloc.fig|.gif` are hand-tracked legacy image sources.
- Markdown is **ExDoc/`ex_doc_wrapper` dialect**: `#`/`##` headings, fenced code blocks, `{: .note }`/`{: .warning }` admonitions, and erlang autolinks (`m:Module`, `m:Module#func/arity`, `t:type`, `c:callback`). Broken links/wrong autolinks surface as ExDoc warnings (treated as errors by default — `EX_DOC_WARNINGS_AS_ERRORS`).
- Keep `guides/erl_ext_dist.md` / `erl_dist_protocol.md` consistent with each other and with the driver/NIF API docs when the formats change.
- Do **not** duplicate VM-internals notes here — they live in `erts/emulator/internal_doc/` and are surfaced, not copied (see Design Decisions).

## Design Decisions
- **Internal VM design docs stay next to the code** (`erts/emulator/internal_doc/*.md`) and are surfaced into the erts HTML doc set by the `Path.wildcard("../emulator/internal_doc/*.md")` term in `docs.exs` `extras` (grouped as **"Internal Docs"**). Only their image assets are copied into `erts/doc/assets/` at build time. Rationale: the design notes belong with the source they describe and must not be duplicated/diverged here.
- **`erlang` module docs are split into fragments**: the big `system_info/1`, `port_info/2` reference text lives in this dir (`src/*.md`) and is embedded into `erts/preloaded/src/erlang.erl` via `-doc {file,...}`, keeping that already-huge source module readable.
- **Single source, two renderings**: the same `references/*.md` and `src/*.md` feed both the HTML doc set (ExDoc) and the OS man pages (`markdown_to_man.escript`), so guides/references never diverge between formats.
- `EPUB=false` for erts (no EPUB output for the runtime-system docs).

## Known Issues
- `src/erl_fix_alloc.fig` / `src/erl_fix_alloc.gif` are unreferenced legacy image sources (untouched since R13B03) — no current doc points at them.
- `src/.gitignore` still lists legacy docgen artifacts (`ref_man.xml`, `specs.xml`, `part.xml`); `man1/.gitignore` is an empty placeholder.
- Because `ERL_FLAGS="-pz $(ERL_TOP)/erts/ebin"` is required, the HTML doc build needs the erts app compiled/installed first.

## Notes for Agents
- A clean checkout contains **no `html/`, `man1/`, `man3/`, `man4/` content and an empty `assets/`** (only `.gitignore`); generate them via the `assets`/`html`/`man` targets.
- Locating content: operator/CLI manuals → `references/*_cmd.md`; C API manuals → `references/{erl_driver,erl_nif,driver_entry,erts_alloc}.md`; conceptual/theory → `guides/`; `erlang` BIF reference → `src/`. VM-internals design notes are **not** here — see `../emulator/internal_doc/`.
- `notes.md` is the ERTS release-notes file (append-only by release tooling; avoid restructuring).

## Dependencies
- **ExDoc (Elixir)** + the `make/` wrappers: `make/check_ex_doc` (version check; `EX_DOC_VERSION` env is required), `make/ex_doc_wrapper`, `make/get_ex_doc_formats_no_epub.sh`, `make/ex_doc.exs`.
- `escript` (from the built OTP) for `make/markdown_to_man.escript`.
- A configured, compiled `erts` (`ERL_FLAGS="-pz $(ERL_TOP)/erts/ebin"`); mermaid.js is loaded from a CDN at view time only.
- `SOURCE_DATE_EPOCH` (optional) makes the doc build reproducible.

## Test Strategy
- No unit tests here. Validation is the doc build itself: `make docs` (or `html`/`man`) — ExDoc runs with `EX_DOC_WARNINGS_AS_ERRORS` and fails on broken markdown links/undefined references (undefined refs are waived only for `notes.md`).
- Preview: open `html/index.html` after `make html`.

## Routing Table
- `guides/` → User's Guides (conceptual/theory docs for app & system programmers): distribution protocol, external term format, drivers, tty, time, match specs, crash dumps, abstract format, inet config, alt carrier/discovery
- `references/` → Reference manuals: operator command manuals `*_cmd.md` → man1, and C API manuals `erl_driver`/`erl_nif`/`driver_entry`/`erts_alloc` → man3
- `src/` → hand-written `erlang` BIF doc fragments (`erlang_system_info.md`, `erlang_port_info.md`) + legacy `erl_fix_alloc.fig|.gif`; `man3` output lands here
- `assets/` → generated copy target for images/`.erl` examples (git-ignored); source images live in `../emulator/internal_doc/assets/`
- `man1/` → generated `man1` output dir (git-ignored; only a `.gitignore` is tracked)
- `docs.exs` → ExDoc config for the erts app (extras order, groups, annotations)
- `notes.md` → ERTS release notes (auto-included by `make/ex_doc.exs`)
- `Makefile` → build entry (`include ../vsn.mk`, `include $(ERL_TOP)/make/doc.mk`)

## See Also
- `../emulator/internal_doc/` → VM-internals design docs + their `assets/`, surfaced here via `docs.exs` (master copy — edit there, not here)
- `../preloaded/src/erlang.erl` → consumes `src/*.md` via `-doc {file,...}`
- `../../lib/kernel/examples/{gen_tcp_dist,erl_uds_dist}/` and `../example/time_compat.erl` → example sources copied into `assets/`
- `../../make/doc.mk`, `../../make/ex_doc.exs`, `../../make/ex_doc_wrapper`, `../../make/markdown_to_man.escript` → the shared doc build machinery
- `../../system/doc/` → system-level/index docs; `../../doc/` → top-level index (`INDEX_DIR`)
