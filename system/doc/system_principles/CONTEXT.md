# System Principles Guide

## Intent
User guide on building and operating complete Erlang/OTP *systems* (as opposed to the language itself): starting/stopping the runtime, boot scripts, code-loading strategy, error logging, creating and upgrading target systems (releases), the OTP version scheme, and the support/compatibility/deprecation/removal policy.
The guide is a flat set of ex_doc "extras" chapters of the `system` app documentation — it is NOT an application and has no code, build outputs, or subdirectories of its own.

## API Surface (page inventory — one line each)
- `system_principles.md` — landing chapter (explicit anchor `#system-principles`): starting/stopping the system (`erl`, `m:init`), boot scripts (`#BOOTSCRIPT`, `#default_boot_scripts`), code-loading strategy (`#code_loading`), and the file-type table (`.erl`/`.hrl`/`.rel`/`.app`/`.script`/`.boot`/`.config`/`.appup`/`relup`).
- `error_logging.md` — runtime error reporting and Logger (`#error-logging`); summarises Kernel Logger and SASL error logging.
- `create_target.md` — creating, installing and upgrading a target system (`#creating-upgrading-target-system`) using SASL's `target_system` module; embeds a full inline listing of that module (`#listing-of-target-system`).
- `upgrade.md` — upgrading when Erlang/OTP itself changes (`#upgrade-section`): `restart_new_emulator` vs `restart_emulator`, and apps that do not allow code upgrade.
- `versions.md` — OTP/application version scheme (`#versions-section`): explicit anchors `#set-of-applications`, `#version_scheme`, `#release-candidates`, `#application-version`, `#releases_and_patches`, `#otp_versions_tree`, `#otp_17_0_app_versions`; contains 5 mermaid diagrams of the version tree/order.
- `misc.md` — support, compatibility, deprecation and removal policy (`#supported_releases`); links out to the `general_info` pages.

## External file dependencies (content is coupled to files OUTSIDE this directory)
- **`versions.erl` asset (build-time copy).** `versions.md` links to `assets/versions.erl` with a raw relative Markdown link — it is *not* an ex_doc autolink and does not exist in the source tree. `system/doc/Makefile` defines `ASSETS_SRC = <glob */assets/*.{gif,png,svg}> $(ERL_TOP)/lib/runtime_tools/src/versions.erl`, and the `assets.copy` rule runs `cp -f $(ASSETS_SRC) assets/`. So the file is a copy of `../../lib/runtime_tools/src/versions.erl`; the link resolves because `make/ex_doc.exs` maps `system/doc/assets` into the output under `assets/`. **Edit the source in `lib/runtime_tools/src/versions.erl`**, never the copy.
- **`assets/versions.erl` is NOT git-ignored.** `system/doc/assets/.gitignore` ignores only `*.png`/`*.gif`/`*.svg`; nothing ignores `*.erl` there, so after a doc build the copy appears as an untracked file (it just never exists in a clean checkout). The only tracked asset is the ex_doc logo `erlang-logo.png`; the `assets.copy` stamp is ignored via `system/doc/.gitignore`.
- **`create_target.md` embeds a manual copy of `lib/sasl/examples/src/target_system.erl`** (the `#listing-of-target-system` section). It is hand-maintained and has DRIFTED from the source: the example source now carries an extra `-compile([{nowarn_possibly_unsafe_function, {file, consult, 1}}]).` line and a trailing `%module` marker that the listing lacks (the rest matches modulo whitespace). Sync it manually when the example changes.

## Constraints
- Every page must be registered in `system/doc/docs.exs` under `extras:` (all 6 are present, each as `"system_principles/<file>.md": []`). Forgetting this drops the page from the build.
- The sidebar group title "System Principles" comes from the line `system_principles:System Principles` in `system/doc/guides`; `docs.exs` `groups_for_extras` turns each `name:Title` line into `{Title, ~r/#{name}/}`, so the regex `~r/system_principles/` selects these pages.
- Keep the leading HTML-comment copyright header (`%CopyrightBegin% … SPDX-License-Identifier: Apache-2.0 … %CopyrightEnd%`) at the top of every page (ex_doc strips it).
- Markdown dialect is ex_doc: autolinks `m:module`, `e:app:page.md`, `` `code:fun/1` ``; explicit anchors use the form `[](){: #anchor }`; mermaid is supported via the `before_closing_body_tag` hook in `make/ex_doc.exs`.
- `system_principles/*.md` are pulled into the html build by `system/doc/Makefile` `HTML_DEPS=$(foreach guide, $(GUIDES), $(wildcard $(guide)/*.md))` (GUIDES = first column of `system/doc/guides`).

## Known Issues
- **Anchor spelling is mixed** (underscore vs hyphen): `#version_scheme`, `#BOOTSCRIPT` (upper case), `#releases_and_patches`, `#creating-upgrading-target-system`. Outside consumers link to exact spellings — `lib/runtime_tools/src/system_information.erl` → `system_principles.md#BOOTSCRIPT`; `system/doc/reference_manual/code_loading.md` and `lib/crypto/doc/crypto_app.md` → `system_principles.md#code_loading`; `lib/runtime_tools/src/versions.erl` and `lib/runtime_tools/doc/notes.md` → `versions.md#version-scheme` (hyphen) and `versions.md#order-of-versions` (hyphen, no explicit anchor — relies on the heading's auto slug). Do not rename anchors without grepping the whole tree.
- **Stale inline listing:** the `target_system.erl` listing in `create_target.md` has drifted from `lib/sasl/examples/src/target_system.erl` (see External file dependencies).
- **mermaid hidden in epub:** `make/ex_doc.exs` sets `display:none` for `pre code.mermaid` in the `:epub` output, so the version-tree;diagrams in `versions.md` do not appear in the epub build.

## Notes for Agents
- Leaf node: 6 flat Markdown files, no subdirectories — do not try to delegate further.
- This content deliberately OVERLAPS other guides/app owners; edit the owning copy, not this summary:
  - Release handling detail (`.rel`/`.appup`/`relup`/`.script` formats, `systools`, workflow) → `system/doc/design_principles/release_handling.md`, `release_structure.md`, and man pages `lib/sasl/doc/references/{rel,appup,relup,script}.md`. Pages here only summarise/point to them.
  - Logger / error-logging API → `lib/kernel/doc/guides/logger_chapter.md` and the `m:logger` manual; SASL-specific legacy behaviour → `lib/sasl/doc/guides/error_logging.md`.
  - OTP/application version scheme → the `m:versions` module `lib/runtime_tools/src/versions.erl` (whose moduledoc links back here) and the generated data file `otp_versions.table` at the repo root.
  - Support/deprecation lists → the `general_info` extras (`system/doc/general_info/*`, registered in `system/doc/top/docs.exs`), linked as `e:general_info:*` (the `general_info` ex_doc dep is declared in `make/ex_doc.exs`).
- Build the docs with `make docs` (html) from `system/doc` after `configure`; ex_doc is fetched by `make/check_ex_doc`.

## See Also
- `../design_principles/` — OTP Design Principles (release handling, release structure) — owns the `.rel`/`.appup`/`relup` formats
- `lib/sasl/doc/` — SASL release-handling man pages (`rel`, `appup`, `relup`, `script`) and SASL error logging
- `lib/kernel/doc/guides/logger_chapter.md` — Kernel Logger user guide
- `lib/runtime_tools/src/versions.erl` — `versions` module; source of the `assets/versions.erl` copy
- `../general_info/` (+ `../top/docs.exs`) — deprecations / removals / upcoming incompatibilities pages
- `../programming_examples/`, `../getting_started/`, `../reference_manual/` — sibling `system`-app guides
