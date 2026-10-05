# Erlang Reference Manual (`system/doc/reference_manual`)

## Intent
The normative, prose description of the **Erlang programming language itself** — not of the OTP libraries or of the runtime implementation.
Aimed at readers who already program; it explains language constructs in text and examples rather than with a formal specification, and is explicitly **not** a tutorial.
One of the "User's Guides" rendered by ex_doc; appears in the sidebar group **"Erlang Reference Manual"**.
This is a docs-only directory: no code, no build products.

## API Surface (page inventory, 20 `.md` files)
Ordering below is topical, not the sidebar order (see Constraints).

Landing / overview
- `reference_manual.md` — guide landing page ("Introduction"): purpose, prerequisites, document conventions ("sequence"/"list"), pointer to `m:erlang` for the complete BIF list, reserved words.

Lexical & syntactic foundation
- `character_set.md` — character set and source-file encoding (UTF-8).
- `data_types.md` — terms, numbers, atoms, bit strings/binaries, references, funs, port ids, pids, tuples, maps, lists, (triple-quoted) strings, sigils, records, native records, booleans, escape sequences, type conversions.
- `patterns.md` — very short overview of pattern matching; defers detail to `expressions.md`.
- `expressions.md` — the largest page (~69 KB): evaluation, variables, patterns, match operator, function calls, `if`/`case`/`maybe`, `send`/`receive`, comparisons, arithmetic/boolean/short-circuit, list & map expressions, bit syntax, funs, `catch`/`throw`, `try`, blocks, comprehensions, guard sequences/expressions, operator precedence.

Modules & organisation
- `modules.md` — module syntax, (pre-defined) module attributes, behaviour attribute, record/native-record/preprocessor attributes, file/line, types & specs, documentation attributes, `-feature`, comments, `module_info/0,1`.
- `documentation.md` — in-source documentation: moduledoc & doc metadata, doc signatures, links in markdown, visibility, compiling and rendering with ExDoc/rebar3.
- `ref_man_functions.md` — function declaration syntax, evaluation, tail recursion, BIFs.
- `macros.md` — preprocessor: file inclusion, `-define`/`-undef`, predefined macros, overloading, conditional compilation, `-feature`, `-error`/`-warning`, argument stringification.
- `ref_man_records.md` — classic tuple-backed records.
- `ref_man_native_records.md` — native (map-backed) records, exporting/importing, guards/patterns, reflection, shell, nested records, hot-code-update field add/delete.

Types & specifications
- `typespec.md` — Erlang type language, syntax, built-in type redefinition, user-defined type declarations, record-field types, function specifications.
- `opaques.md` — opaque type aliases.
- `nominals.md` — nominal types: rationale, syntax, type-checking rules.
- `features.md` — the selectable-feature concept, lifecycle, enabling/disabling, existing features (`maybe_expr` approved, `compr_assign` experimental).

Runtime & process semantics
- `ref_man_processes.md` — processes, creation, registered names, aliases, termination, signals (sending/receiving, priority messages), links, exit signals, monitors, process dictionary.
- `errors.md` — terminology (run-time/generated errors), exceptions & stacktrace, run-time error handling within/between processes, exit reasons.
- `distributed.md` — distributed systems, nodes, connections, `epmd`, hidden & dynamic node names, C nodes, security, distribution BIFs/flags/modules.
- `ports.md` — ports and port drivers, port BIFs.
- `code_loading.md` — compilation, code loading, code replacement, `-on_load`.

Assets
- `assets/prio-msg-recv.png` (+ `.license` sidecar) — used only by `ref_man_processes.md` (priority-message-reception section).

## Constraints
- **Registration**: every `.md` here MUST be listed in `system/doc/docs.exs` under `extras:` (paths prefixed `reference_manual/`). An unlisted file is silently not published / not built. All 20 files are currently registered.
- **Sidebar group title**: comes from the line `reference_manual:Erlang Reference Manual` in `system/doc/guides`; `system/doc/docs.exs` builds `groups_for_extras` by reading `guides` and turning each `name:title` into `{title, ~r/name/}`. Do not touch that machinery from here.
- **Ordering / navigation**: there is NO explicit "next/previous" nav in the files. The sidebar order within the group follows the order of entries in `docs.exs` `extras:`. To add or reorder a page: create the `.md` here AND insert/move its `"reference_manual/<name>.md": []` entry in `docs.exs`.
- **Copyright header**: every `.md` starts with an HTML-comment `%CopyrightBegin% … %CopyrightEnd%` block carrying `SPDX-License-Identifier: Apache-2.0` and the Ericsson copyright line.
- **Cross-references**: use ex_doc autolinks — `m:module`, `m:module#function`, `t:type/arity`, `f:fun/arity`, `e:app:page.md`, `e:app:page.md#anchor` (this guide links out mainly to `e:erts:*`, `e:system:*`, `e:stdlib:*`, `e:ssl:*`). Intra-guide links are relative: `page.md` or `page.md#anchor`. Do NOT hardcode `https://www.erlang.org/doc/...` URLs.
- **Anchors**: explicit anchors are written `[](){: #anchor }`, either on their own line or inline as a list-item marker (e.g. `- **[](){: #run-time-errors } Run-time errors**`). Link targets must match an explicit anchor or an auto-generated heading slug.
- **Images**: keep them in `assets/` and reference as `assets/<name>` (path relative to the doc root). The `assets.copy` target in `system/doc/Makefile` (via `ASSETS_SRC`) copies every guide's `assets/*.{png,gif,svg}` FLAT into `system/doc/assets/`, so filenames must be globally unique across guides. Non-image asset files (and images lacking an embedded header) get a `<file>.license` REUSE sidecar.
- **Do not edit** `system/doc/Makefile`, `make/doc.mk` or `make/ex_doc.exs` from here; they are the guide-shared build machinery.
- Markdown is ex_doc/GFM. Mermaid is enabled globally (hook in `make/ex_doc.exs`) but this guide uses no diagrams.

## Known Issues
- **Doctest coupling (high risk for edits)**: the code examples in `ref_man_native_records.md` are executed as doctests. `lib/stdlib/test/records_SUITE.erl` (`doctests_native_records/1`) runs `ct_doctest:file(<data_dir>/ref_man_native_records.md, [{skipped_blocks, 13}])`; `lib/stdlib/test/Makefile` installs the file via `DOCTEST_NATIVE_RECORDS_MARKDOWN`. Changing/adding example blocks can break the stdlib test suite and may require updating the skipped-block count.
- No other `reference_manual/*.md` file is referenced outside `system/doc/` by tests; `HOWTO/DOCUMENTATION.md` and `HOWTO/DEVELOPMENT.md` link into `documentation.md`/`errors.md` (doc content, not tests).
- Flat asset copying means a same-named file in another guide's `assets/` collides.

## Notes for Agents
- Landing page is `reference_manual.md`; the biggest/deepest pages are `expressions.md`, `ref_man_processes.md`, `data_types.md`, `typespec.md`, `ref_man_native_records.md`, `documentation.md`.
- When editing anchors or headings, grep the whole guide for `](page.md#anchor)` and `](#anchor)` before renaming — links are plain relative markdown, not autolinks, and nothing validates them at build time.
- Build/preview of these docs is driven from `system/doc` (ex_doc via `make/docs.mk`); there is no per-file test here.

## See Also
- `../CONTEXT.md` (parent, `system/doc`) — ex_doc build, `docs.exs`, `guides`.
- `../design_principles/`, `../system_principles/`, `../getting_started/`, `../programming_examples/`, `../efficiency_guide/`, `../tutorial/` — sibling guides that this guide links out to.
- `../../ert/CONTEXT.md`-equivalent (`erts/doc/`) and `lib/*/doc/` — ERTS/stdlib/ssl pages referenced via `e:` autolinks.
- `make/ex_doc.exs` — global ex_doc config: autolink/`groups_for_extras`/mermaid.
- `lib/stdlib/test/records_SUITE.erl`, `lib/stdlib/test/Makefile` — doctest of `ref_man_native_records.md`.
- `HOWTO/DOCUMENTATION.md`, `HOWTO/DEVELOPMENT.md` — authoring guidance that points at `documentation.md`/`errors.md`.

## Related paths
- `./assets/` — images used by this guide (copied flat into `system/doc/assets/` at build).
- `../../system/doc/docs.exs` — extras registry and sidebar grouping.
- `../../system/doc/guides` — group titles.
