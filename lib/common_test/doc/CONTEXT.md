# common_test — Documentation

## Intent
Documentation sources for the OTP `common_test` application: the user-guide chapters, the `ct_run` command reference, the application overview page and the release notes.
All hand-written content is markdown; the Erlang module reference pages (`ct`, `ct_*`, `test_server_*`, …) are NOT stored here — they are generated from `-moduledoc`/`-doc` attributes in `../src/*.erl`.

## API Surface / Files
- `Makefile` — thin wrapper: includes `../vsn.mk` (`VSN=$(COMMON_TEST_VSN)`) and `$(ERL_TOP)/make/doc.mk`; sets `APPLICATION=common_test`. All real logic lives in `make/doc.mk`.
- `docs.exs` — the only local ExDoc config. It sets `extras:` in publication order (see below); everything else (theme, groups, deps) comes from `$(ERL_TOP)/make/ex_doc.exs`.
- `common_test_app.md` — hand-written application overview page ("A framework for automated testing of any target nodes"), the app landing page.
- `references/ct_run_cmd.md` — hand-written `ct_run` CLI reference; the `_cmd` suffix makes it a man page.
- `notes.md` — Release Notes for the `common_test` application (per-version "Fixed Bugs and Malfunctions" / "Improvements and New Features" with `Own Id`/`Aux Id` + PR links), newest version first; ~3,700 lines / ~138 KB.
- `assets/` — images used by the guides (`config.gif`, `html_logs.gif`, `tc_execution.gif`).
- `src/filestruct.gif` — orphan image, unreferenced by any markdown in the repo.
- `guides/` — user-guide chapters (markdown, one `# Title` per file).

## Build pipeline
Invoke with `make docs` (or `make html` / `make man`) in this directory; the top-level `make docs` recurses through `lib/Makefile` (`BUILD_ALL=1`) and reaches here.
- `html` / `chunks` → ExDoc. Depends on `$(APP_EBIN_DIR)/*.beam`, `*.md`, `*/*.md`, `assets/*`, `docs.exs` and `make/ex_doc.exs`; runs `make/ex_doc_wrapper` → output in `./html/`.
  Requires the app to be compiled first (the `.beam` files are inputs) and will offer to download the pinned `ex_doc` (`make/check_ex_doc`, `make/ex_doc.sha256sum`).
- `man` → `make/markdown_to_man.escript` converts: `references/*_cmd.md` → `man1/ct_run.1`; `src/*.md` + `references/*.md` + every `../src/**/*.erl` that does NOT say `moduledoc false.` → `man3/*.3`; `*_app.md` → `man6/common_test.6`.
  `references/*_cmd.md` is filtered out of the man3 set, so `ct_run_cmd.md` yields only man1.
- `release_docs_spec` / `release_html_spec` / `release_man_spec` install into `$(RELEASE_PATH)/lib/common_test-$(VSN)/doc/html` and `$(RELEASE_PATH)/man/man{1,3,6}`; `INDEX_DIR` only supplies the homepage URL of the combined OTP doc site.
- ExDoc extra pages = local `docs.exs` `extras` ++ `Path.wildcard("*.md")` ++ `Path.wildcard("{guides,references,internal_docs}/*.md")` (from `make/ex_doc.exs`), de-duplicated. So `notes.md` and `common_test_app.md` are picked up by the wildcard even though only `common_test_app.md` is listed explicitly; the list controls ORDER, not inclusion.
- Grouping in the sidebar is by regex in `make/ex_doc.exs`: `guides/` → "User's Guides", `references/*_cmd.md` → "Command Line Tools", other `references/` → References.

## Publication order (`docs.exs` extras)
1. `introduction.md` — scope, prerequisites, framework overview.
2. `basics_chapter.md` — Common Test Basics: terms, test suite/test case structure, the CT framework.
3. `getting_started_chapter.md` — first test run walkthrough (references `assets/*.gif`).
4. `install_chapter.md` — Installation (short; target of the link from `ct_run_cmd.md`).
5. `write_test_chapter.md` — Writing Test Suites: callbacks, `init_per_*`/`end_per_*`, config handling, skipping.
6. `test_structure_chapter.md` — Test Structure; nested subdirectories of suites.
7. `example_chapter.md` — Examples and Templates.
8. `run_test_chapter.md` — Running Tests and Analyzing Results (longest chapter: `ct_run`/`ct:run_test`, test specs, logging, HTML logs, result analysis).
9. `config_file_chapter.md` — External Configuration Data.
10. `cover_chapter.md` — Code Coverage Analysis via `cover`.
11. `ct_master_chapter.md` — Large-scale/distributed test runs with `ct_master`.
12. `event_handler_chapter.md` — Event Handling (`gen_event` handlers, `ct_event`/test_server events).
13. `dependencies_chapter.md` — Dependencies between test cases and suites.
14. `ct_hooks_chapter.md` — Common Test Hooks (`ct_hooks` behaviour).
15. `why_test_chapter.md` — Some Thoughts about Testing (rationale).
16. `ct_property_test_chapter.md` — Property-testing support (`ct_property_test`, QuickCheck/PropEr).
17. `references/ct_run_cmd.md` — `ct_run` command line reference.
18. `common_test_app.md` — application overview page.

## Constraints
- Prose only — never reword, reflow or "clean up" guide text as a side effect of an unrelated change; edit content deliberately.
- Adding or removing a chapter requires updating the `extras:` list in `docs.exs` to keep publication order deterministic (the wildcard would otherwise reorder/newly add nothing but ordering would be undefined).
- Image files belong in `assets/` and are referenced as `assets/<name>.gif`; the whole `assets/` dir is an ExDoc build dependency.
- Do not hand-edit generated output (`html/`, `man1/`, `man3/`, `man6/`) — root `.gitignore` ignores `/lib/*/doc/html` and `*.1`; man3/man6 outputs are regenerated.
- `notes.md` is append/release-generated content — do not restructure it; new entries are added at the top per release by the release process.
- Module reference pages must be edited in `../src/*.erl` (`-moduledoc`/`-doc`), never here.

## Known Issues
- `src/filestruct.gif` is a leftover from the pre-ExDoc XML documentation era (the `.xml` sources were removed); nothing references it. It exists only because gif cleanup missed it.
- `notes.md` is large (~138 KB) — do not read it wholesale; grep for the version or `Own Id` you need.
- Doc builds mutate this directory (`html/`, `man*/`); avoid running them when a clean tree is required.

## Dependencies
- Elixir/ExDoc (version pinned by `make/ex_doc_vsn` + `make/ex_doc.sha256sum`; downloaded on first use via `make/check_ex_doc`), plus `escript`/`erl` from the tree for `markdown_to_man.escript`.
- Requires `lib/common_test/ebin/*.beam` (i.e. the app must be built) before `make docs`.

## Test Strategy
Documentation is not unit-tested; correctness is enforced by the doc build itself (`EX_DOC_WARNINGS_AS_ERRORS` defaults to true, so broken intra-doc references (`m:`, `e:`, relative links) fail the build). `notes.md` is exempt from undefined-reference warnings.

## Routing Table
| Path | Description |
| --- | --- |
| `guides/` | User-guide chapters (16 markdown files, publication order above) |
| `references/` | `ct_run_cmd.md` — `ct_run` command-line reference (becomes `man1/ct_run.1`) |
| `assets/` | GIF images referenced from `guides/getting_started_chapter.md` |
| `src/` | Only the orphan `filestruct.gif` — no doc sources here despite the name |
| `docs.exs` | ExDoc extras ordering (edit when chapters change) |
| `common_test_app.md` | Application overview/landing page (also source of `man6/common_test.6`) |
| `notes.md` | `common_test` Release Notes (per-version, newest first) |
| `Makefile` | Wrapper around `$(ERL_TOP)/make/doc.mk` |

## See Also
- `../src/` — the modules whose `-doc`/`-moduledoc` attributes generate the API reference pages (write there, not here).
- `../internal_doc/` — non-published Common Test documentation (sibling — read-only, escalate writes to parent).
- `../test/` — the Common Test test suites (and shared `../test_server/`); sibling, read-only.
- `../../make/doc.mk`, `../../make/ex_doc.exs`, `../../make/markdown_to_man.escript` — the shared doc-build infrastructure this directory depends on.
