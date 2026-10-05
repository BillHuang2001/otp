# EUnit Documentation

## Intent
Documentation sources for the `eunit` application.
Holds the EUnit User's Guide (`guides/chapter.md`) and the application release notes (`notes.md`); everything else is generated build output.
The guide is the authoritative prose reference for EUnit and is linked to from the public module docs in `../src/`.

## API Surface
- `Makefile` — includes `../vsn.mk`, sets `APPLICATION=eunit` and `VSN=$(EUNIT_VSN)`, then includes `$(ERL_TOP)/make/doc.mk` (the shared ExDoc-based doc build). Targets: `docs`, `release_docs_spec` (see `make/doc.mk`).
- `docs.exs` — ExDoc config; the only local setting is `extras: ["guides/chapter.md"]` (extras order determines listing order). `make/ex_doc.exs` automatically appends `*.md` at the doc root (so `notes.md` needs no entry) plus `{guides,references,internal_docs}/*.md`.
- `guides/chapter.md` (~1130 lines) — the entire EUnit User's Guide in one file.
- `notes.md` (~645 lines) — release notes, rendered as the app's Release Notes page.
- `.gitignore` — ignores generated HTML (`*.html`, `stylesheet.css`, `erlang.png`, `edoc-info`).

## Content Map of `guides/chapter.md`
Anchored sections (anchor line precedes the heading): `#Unit_testing`, `#Advantages_of_unit_testing`, `#Terminology`, `#Getting_started` (`#Including_the_EUnit_header_file`, `#Writing_simple_test_functions`, `#Running_EUnit`, `#Writing_test_generating_functions`, `#An_example`, `#Disabling_testing`, `#Avoiding_compile-time_dependency_on_EUnit`).
Then `#EUnit_macros` with `#Basic_macros`, `#Compilation_control_macros`, `#Utility_macros`, `#Assert_macros`, `#Macros_for_checking_output`, `#Macros_for_running_external_commands`, `#Macros_for_instrumentation`, `#Debugging_macros`.
Then `#EUnit_test_representation` with `#Simple_test_objects`, `#Test_sets_and_deep_lists`, `### Titles`, `### Primitives`, `### Control`, `### Fixtures`, `#Lazy_generators`.
23 anchor markers total; each top-level section also has a bullet list linking to its own subsections.

## Constraints
- Anchor convention: an explicit anchor is written as `[](){: #Anchor }` on its own line before the heading, or inline as `_{: #anchor }` at the end of a paragraph. Headings without an explicit anchor are referenced by their lower-cased Emacs-style slug (e.g. `chapter.md#terminology`, `chapter.md#titles`, `chapter.md#fixtures`).
- All in-guide cross-references are written as `chapter.md#Anchor` (relative to the generated page), even though the file physically lives in `guides/` — do not rewrite them to `guides/chapter.md#...`.
- Anchors are case-sensitive strings and the mix of `Camel_Case` and `lower-case` slugs is intentional; renaming/moving a heading breaks links from `../src/*.erl` `-doc` attributes (e.g. `eunit:test/1` links to `chapter.md#EUnit_test_representation`).
- Never edit generated HTML; docs are produced by ExDoc from these markdown sources.
- Release notes are appended per release: a new top-level `## Eunit <major>.<minor>[.<patch>]` section at the top with the `### Improvements and New Features` and/or `### Fixed Bugs and Malfunctions` subsections; each bullet ends with `Own Id: OTP-NNNNN` (optional `Aux Id: ...`).
- The macro documentation under `## EUnit macros` must stay in sync with `../include/eunit.hrl` (49 `-define`s); every documented macro corresponds to a define there. Newest example: `?randomDelay/3`.
- The `## EUnit test representation` section must stay in sync with the `tests()` / `simple_test()` types in `../src/eunit_data.erl` (simple test objects, control descriptors like `{timeout,...}`/`{inorder,...}`, fixtures).
- Only `eunit` and `eunit_surefire` have public module docs; all other `../src/*.erl` are `-moduledoc false` — do not document internals here.
- Keep the `%CopyrightBegin%`/`%CopyrightEnd%` SPDX header block on every file; `guides/chapter.md` additionally carries a dual `Apache-2.0 OR LGPL-2.1-or-later` license and third-party copyright (Mickaël Rémond, Richard Carlsson).

## Design Decisions
- One single guide file rather than a `guides/` set of chapters; it is registered as the sole `extras` entry so it appears as a standalone "User's Guide" page.
- `notes.md` is not listed in `docs.exs` because `make/ex_doc.exs` globs root `*.md` automatically.
- The guide is HTML-only: `make/doc.mk` derives man pages from `references/*.md`, `src/*.md` and `*_cmd.md`, not from `guides/*.md`, so it never becomes a `man` page.
- Existing prose uses ` ```text ` fenced blocks for Erlang snippets (legacy formatting, kept for fidelity rather than ` ```erlang `).

## Notes for Agents
- Build the docs with `make docs` in this directory (requires `ex_doc`, which the build downloads on demand via `make/check_ex_doc`); this is not part of the normal `make` build.
- Undefined cross-reference warnings from `notes.md` are suppressed (`skip_undefined_reference_warnings_on: ["notes.md"]` in `make/ex_doc.exs`), so broken links inside release notes will not fail the doc build.
- The guide's "An example" section inlines the same `fib` module that ships as `../examples/fib.erl` (plus `tests.txt`); the examples are installed to `$(RELSYSDIR)/doc/examples` by `../examples/Makefile`.

## Test Strategy
- No test suite lives in this directory; correctness is validated only by the ExDoc build (reference checks, `EX_DOC_WARNINGS_AS_ERRORS`) and the release doc build.
- There is no automated check that the macro list or the test-representation grammar still match `../include/eunit.hrl` / `../src/eunit_data.erl` — this must be verified manually when they change.

## See Also
- Parent `../` (`lib/eunit/`) — `src/`, `include/`, `test/`, `examples/`, `vsn.mk`.
- `../src/eunit.erl`, `../src/eunit_surefire.erl` — the only publicly documented modules; their `-doc` attributes link into `chapter.md`.
- `../include/eunit.hrl` — macro definitions the guide documents.
- `../src/eunit_data.erl` — `tests()` grammar the test-representation section documents.
- `../../../make/doc.mk` and `../../../make/ex_doc.exs` — shared ExDoc build and conventions (read-only, escalate changes to the parent).
