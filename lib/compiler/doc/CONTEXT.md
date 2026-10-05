# Compiler Application — Documentation Build

## Intent
Builds and ships all published documentation for the `compiler` OTP application (ExDoc-generated HTML chunks, plus man pages derived from markdown references).
Unlike most OTP apps there is **no `doc/src/` directory**: module documentation lives inline in the sources (`-moduledoc` / `-doc` / `-doc` attributes in `../src/*.erl`) and ExDoc extracts it from the compiled `.beam` files.
Extra prose pages are pulled in from `../internal_doc/`.

## API Surface
- `Makefile` — thin wrapper: includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(COMPILER_VSN)`), then `$(ERL_TOP)/make/doc.mk` with `APPLICATION=compiler`.
  Targets (`docs`, `html`, `chunks`, `man`, `release_docs_spec`, `clean_docs`, …) and the actual ExDoc invocation all come from `make/doc.mk` — nothing doc-build-specific is defined here.
- `docs.exs` — the app-local ExDoc config, merged over `make/ex_doc.exs` by the build.
  - `extras:` lists `../internal_doc/beam_ssa.md` and `../internal_doc/ssa_checks.md` (order defines listing order; these are published under the "Internal Docs" group by the shared config).
  - `skip_code_autolink_to: ["compile:options/0", "erl_id_trans"]` — suppresses undefined-reference warnings for those targets.
  - `groups_for_modules:` puts `:cerl`, `:cerl_clauses`, `:cerl_trees` in an "Internal Modules" group.
- `notes.md` — Compiler application release notes (the only release-notes file for this app), newest version first, one `## Compiler <vsn>` section per release with `### Fixed Bugs and Malfunctions` / `### Improvements and New Features` entries carrying OTP-Jira ids and GH issue/PR link references at the end of each version section.
  It is picked up automatically as an ExDoc extra via `Path.wildcard("*.md")` in `make/ex_doc.exs`, and that same shared config sets `skip_undefined_reference_warnings_on: ["notes.md"]` so its link references do not fail the doc build.

## Constraints
- Add or change module docs in `../src/*.erl` attributes — there is no per-page markdown source here for module documentation.
- Do not restructure `notes.md` (keep the version → section → bullet layout, newest version first); new release notes are prepended per release.
- `html/` is generated output (`/lib/*/doc/html` is git-ignored); never hand-edit or commit it.
- Edits to `docs.exs` must stay valid Elixir keyword-list syntax; the file is evaluated by ExDoc together with `make/ex_doc.exs`.
- `extras` paths in `docs.exs` are relative to this directory (`../internal_doc/...`).

## Design Decisions
- Documentation is generated from inline attributes rather than a `doc/src/` tree so module docs stay next to the code they describe (migration from XML doc sources to markdown/ExDoc).
- The deeper compiler-internals documents (`beam_ssa.md`, `ssa_checks.md`, `cerl-notes.md`) live in `../internal_doc/` because they describe compiler passes, not user-facing APIs; only the first two are exported as ExDoc extras.
- Build logic is centralised in `make/doc.mk` + `make/ex_doc.exs` so all OTP apps share one ExDoc configuration; only app-specific overrides belong in `docs.exs`.

## Notes for Agents
- `../internal_doc/cerl-notes.md` is **not** listed in `docs.exs` extras — it is referenced only from a source comment in `../src/cerl_clauses.erl`, so it is not part of the generated docs.
- `contributing.md`, `assets/`, `guides/`, `references/` and `*_cmd.md` conventions used by other apps are absent here; the compiler app publishes only the module docs, the two internal-doc extras and `notes.md`.
- Doc build requires ExDoc (version supplied via `EX_DOC_VERSION`); `make docs` first runs `check_ex_doc` and may prompt to download ExDoc when targeting HTML.

## Dependencies
- ExDoc (Elixir/escript) driven through `$(ERL_TOP)/make/ex_doc_wrapper`; `ERL_FLAGS="-pz $(ERL_TOP)/erts/ebin"` is set by `make/doc.mk`.
- `ERL_TOP` must be set (build is always run from within the OTP build tree).

## Test Strategy
- No test suites live here; validation is `make docs` in this directory (fails on ExDoc warnings/undefined references).
- Doc errors surface at release time in CI; there are no unit tests for doc content.

## See Also
- `../src/` — Erlang sources holding the inline module documentation.
- `../internal_doc/` — compiler-internals markdown (two files exported as doc extras).
- `../vsn.mk` — supplies `COMPILER_VSN` used as the doc version.
- `../../make/doc.mk`, `../../make/ex_doc.exs` — shared OTP documentation build machinery.
- `../CONTEXT.md` — compiler application overview.
