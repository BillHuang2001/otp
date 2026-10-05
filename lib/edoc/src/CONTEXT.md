# edoc/src — EDoc Sources

## Intent
Sources of the `edoc` OTP application: EDoc, the Erlang program documentation generator.
EDoc reads Erlang source files, extracts legacy `%%` edoc comments (and `@`-tags in them), builds an internal XML data model of each module, and renders it through a pluggable *doclet* + *layout* pair (HTML by default, EEP-48 chunks or EEP-59 Markdown alternatively).
The directory is flat — no subdirectories; every module is a top-level `.erl` here.

## API Surface
Public (documented, no module-level `@private`) — the entry points users call:

Public drivers / front-ends
- `edoc.erl` — the main user-facing API: `files/1,2`, `application/1,2,3`, `toc/1,2,3`, `run/2`, `file/1,2`, `read/1,2`, `layout/1,2`, `get_doc/1,2,3`, `read_comments/1,2`, `read_source/1,2`; exports types `module_meta`, `env`, `comment`, `entry`, `entry_data`, `function_name`, `tag`, `edoc_module` (some of these `@deprecated`).
- `edoc_run.erl` — `erl -run edoc_run ...` interface (`file/1` deprecated, `application/1`, `files/1`, `toc/1`); the functions drive the emulator to terminate with a success/failure exit status, so no trailing `-s init stop` is needed.
- `edoc_extract.erl` — extracts documentation data from sources: `source/3,4,5`, `header/3,4,5`, `file/4`, `text/4` (the module turned into `edoc` XML blocks).

Doclet behaviour & doclets (each doclet implements the `edoc_doclet` behaviour)
- `edoc_doclet.erl` — the standard (HTML) doclet; defines `-callback run(command(), context()) -> ok` and exports types `command`, `context`, `doclet_gen`, `doclet_toc`.
- `edoc_doclet_chunks.erl` — doclet that produces EEP-48 doc chunks.
- `edoc_doclet_markdown.erl` — doclet that converts an application's docs to EEP-59 Markdown (pairs with `edoc_layout_chunks`).

Layout modules
- `edoc_layout.erl` — the standard HTML layout; `module/2`, `overview/2`, `type/1` plus helper renderers `copyright/1`, `version/1`, `since/1`, `authors/1`, `references/1`, `sees/1`, `todos/1`; declares `-callback module(edoc:edoc_module(), _) -> binary()`.
- `edoc_layout_chunks.erl` — converts the EDoc XML for a module/overview into an EEP-48 `#docs_v1{}` chunk; exports `module/2`, `overview/2` and the `docs_v1` types.
- `edoc_html_to_markdown.erl` — converts `application/html+erlang` (EEP-48 chunk elements) to ExDoc Markdown: `convert_html/2,3`, `convert_xml/2,3`.

Internal modules (module-level `@private`; not part of the published API but shared across the app)
- `edoc_cli.erl` — command-line front-end (`main/1`) used by the `bin/edoc` escript.
- `edoc_data.erl` — builds the EDoc external XML data structure: `module/4`, `overview/4`, `type/3`, `hidden_filter/2`, `get_all_tags/1`, `get_entry/2`, `get_tags/2`.
- `edoc_specs.erl` — bridge to Erlang specs/types (`type/2`, `spec/1`, `dummy_spec/1`, `docs/2`, `add_type_data/4`, `tag/1`, `is_tag/1`).
- `edoc_types.erl` — datatype representation (`is_predefined/2`, `is_new_predefined/2`, …).
- `edoc_macros.erl` — macro expansion (`expand_tags/3`, `std_macros/1`, `check_defs/1`).
- `edoc_refs.erl` — representation/handling of EDoc object references (`app/1,2`, `module/1,2,3`, function refs; type `t/0`).
- `edoc_wiki.erl` — wiki/XML text expansion (`parse_xml/2`, `expand_text/2`).
- `edoc_scanner.erl` — tokeniser for edoc comments/typespecs (`string/1,2`, `format_error/1`).
- `edoc_tags.erl` — the tag table and line scanning (`tags/0,1`, `tag_names/0`, `tag_parsers/0`, `scan_lines/2`; type `tag_flag/0`).
- `edoc_lib.erl` — utilities (`get_first_sentence/1`, `parse_expr/2`, `parse_contact/2`, `escape_uri/1`, `join_uri/2`, env creation `get_doc_env/3`, …).
- `edoc_report.erl` — verbosity/error/warning reporting.

Grammar & headers
- `edoc_parser.yrl` — Yecc grammar (adapted from Sven-Olof Nyström's typespec parser); root `start` covers `start_spec`, `start_throws`, `start_typedef`, `start_ref` — i.e. typespecs, typedefs, references and throws declarations.
- `edoc.hrl` — internal records (`#module`, `#env`, `#comment`, `#entry`, `#tag`) plus app-wide macros (`?DEFAULT_DOCLET`, `?DEFAULT_LAYOUT`, `?INFO_FILE`, `?OVERVIEW_FILE`, …); it `-include`s the public doclet header.
- `edoc_types.hrl` — internal type-representation records (`#t_name`, `#t_var`, `#t_atom`, `#t_map_field`, …).
- `edoc.app.src` / `edoc.appup.src` — app resource templates (`%VSN%` substituted at build); `applications` = `compiler, kernel, stdlib, syntax_tools`.
- `Makefile` / `files.mk` — build; `files.mk` lists `ERL_FILES`, `HRL_FILES` (including `../include/edoc_doclet.hrl`) and `YRL_FILES`.

## Constraints
- EDoc's *input* is legacy Erlang `%%` comment blocks parsed by `edoc_scanner`/`edoc_parser`/`edoc_tags`/`edoc_wiki`; it does NOT parse `-doc`/`-moduledoc` attributes — EEP-48/`-doc` chunks are an *output* format produced by the chunks doclet/layout and the markdown converters.
- `edoc_parser.erl` is generated from `edoc_parser.yrl` by Yecc at build time — do NOT edit it, and do NOT commit it; `clean` deletes it, and `files.mk` lists it in `ERL_FILES` while the source of truth is `YRL_FILES`.
- Public vs internal is conveyed by module-level `@private` in the module doc: keep it accurate, since it governs what appears in EDoc's own docs.
- Every module compiles with `-Werror` (see `src/Makefile`: `+warn_unused_vars +warn_unused_import +warn_deprecated_guard +nowarn_missing_spec_documented +warn_deprecated_catch -Werror`); unused vars/imports are errors.
- Doclets in this tree deliberately avoid depending on `edoc.hrl` (they use `include/edoc_doclet.hrl` via `-include_lib("edoc/include/edoc_doclet.hrl")`); `edoc_layout_chunks` is an explicit exception that does include `edoc.hrl` and reaches into `#entry{}` records.
- The doclet behaviour contract is fixed: a doclet is any module exporting `run/2` accepting a `command()` (`#doclet_gen{}` | `#doclet_toc{}`) and a `#doclet_context{}`; layouts are pluggable modules called with `module/2` / `overview/2`.
- `edoc.hrl` is included by compiled modules only; `include/edoc_doclet.hrl` is the *public* header shipped for external doclet authors (`?NO_APP` macro kept for backwards compatibility).

## Design Decisions
- Documentation generation is split into three pluggable stages — *extraction* (`edoc_extract`), *data model building* (`edoc_data`), and *rendering* (doclet + layout) — so the renderer can be swapped (HTML, EEP-48 chunks, Markdown) without touching parsing.
- Doclets and layouts are plain modules with fixed function/`run-2` contracts rather than callbacks into a fixed pipeline, enabling third-party doclets/layouts.
- The `chunks` path reuses the EDoc XML model and maps EDoc conventions onto EEP-48 (`@private` → `hidden`), keeping one extraction pipeline for both HTML and chunk output.
- `edoc_html_to_markdown` and `edoc_doclet_markdown` are newer additions (ExDoc/EEP-59 ecosystem) layered on top of the EEP-48 chunk representation.
- `edoc_parser.yrl` is a dedicated Yecc grammar rather than reusing `erl_parse` because EDoc must parse standalone typespecs, typedefs and reference expressions.

## Notes for Agents
- The directory is intentionally flat; do not create subdirectories.
- `edoc_parser.erl` will be absent from a clean tree and appears after a build — it is generated, not source.
- `ERL_DETERMINISTIC=yes` sets `DEP_SKIP_FILES = edoc_parser.erl` in `src/Makefile` to keep dependency generation deterministic.
- `edoc.erl` is NOT `@private` despite grepping hits — its only `@private` mention is a TODO comment; the genuinely internal modules are listed above.
- XHTML in EDoc comments is validated against `../doc/edoc.dtd`.
- Several modules carry `nowarn_deprecated_catch` and `{nowarn_possibly_unsafe_function, ...}` compile options because EDoc intentionally uses `catch` and `list_to_atom`/`binary_to_atom` on documentation-derived input.

## Dependencies
- Erlang apps: `compiler`, `kernel`, `stdlib`, `syntax_tools` (AST manipulation, pretty-printing via `erl_syntax`/`erl_prettypr`, comment scanning via `erl_comment_scan`).
- `xmerl` — XML parser/generator used to build and post-process the EDoc XML model (`xmerl/include/xmerl.hrl`).
- `kernel`'s `eep48.hrl` (`kernel/include/eep48.hrl`) for the EEP-48 doc-chunk path.
- `shell_docs`/ExDoc (EEP-59 Markdown) are the consumers of the markdown/chunk output, not build-time deps.

## Test Strategy
- Common Test suites live in the sibling `../test/` directory (`*_SUITE.erl`), built via `../test/Makefile` and run with `make test` in `../`.
- Verification of end-to-end behaviour (HTML/chunk/markdown generation) is done from those suites plus the app's own docs build; there are no unit tests inside `src/`.

## See Also
- `../include/edoc_doclet.hrl` — public doclet header (`#doclet_context`, `#doclet_gen`, `#doclet_toc`, `?NO_APP`).
- `../bin/edoc` — escript wrapper around `edoc_cli:main/1`.
- `../doc/` — EDoc user manual, references (`references/edoc_cmd.md`), guides and `edoc.dtd`.
- `../test/` — Common Test suites (sibling directory — read-only from here, escalate writes to the parent).
- `../../syntax_tools/` — AST/pretty-printing/comment-scanning helpers; `../../xmerl/` — XML backend.

## Routing Table
This directory is flat, so there are no child nodes to delegate to. Route by module group:
- Public API / drivers → `edoc.erl`, `edoc_run.erl`, `edoc_cli.erl`
- Doclets (behaviour + implementations) → `edoc_doclet.erl`, `edoc_doclet_chunks.erl`, `edoc_doclet_markdown.erl`
- Layout / rendering → `edoc_layout.erl`, `edoc_layout_chunks.erl`, `edoc_html_to_markdown.erl`
- Extraction & data model → `edoc_extract.erl`, `edoc_data.erl`, `edoc_specs.erl`, `edoc_types.erl`, `edoc_macros.erl`, `edoc_refs.erl`, `edoc_wiki.erl`
- Parsing & scanning → `edoc_parser.yrl`, `edoc_scanner.erl`, `edoc_tags.erl`
- Support → `edoc_lib.erl`, `edoc_report.erl`
- Headers / build → `edoc.hrl`, `edoc_types.hrl`, `edoc.app.src`, `edoc.appup.src`, `Makefile`, `files.mk`
