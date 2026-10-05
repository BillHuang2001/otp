# mnesia — Documentation

## Intent
Sources for the Mnesia application documentation: the user's guide, release notes, and the example modules that both appear in the guide and are compiled/run by the test suite.
The mnesia Reference Manual itself is generated from the `-moduledoc`/`-doc` attributes in `../src/` — not from anything in this directory.

## API Surface (areas)
- `docs.exs` — ExDoc config; its `extras:` list alone fixes the published guide order and membership.
- `guides/` — hand-written markdown user's guide chapters: `mnesia_chap1.md`..`mnesia_chap8.md` (there is **no** `mnesia_chap6.md`), `mnesia_overview.md`, and appendices `mnesia_app_a/b/c.md`.
- `notes.md` — release notes / change log, published as the ExDoc "notes" page.
- `src/` — example sources used in the guide and by the test suite: `company.erl`, `company_o.erl`, `bup.erl` (plain compilable modules, **no** `-moduledoc`/`-doc` attributes), record headers `company.hrl`/`company_o.hrl`, sample data `DATA`, `DATA2`, `FRUITS`, and the legacy `company.fig` diagram source.
- `misc/` — internal design notes `implementation.txt` + diagrams (`.png`/`.puml`/`.fig`: async transaction, table loading). NOT published docs.
- `Makefile` — doc build entry; only sets `APPLICATION=mnesia`, `VSN=$(MNESIA_VSN)` and includes `$(ERL_TOP)/make/doc.mk`.

## How the doc build is wired
- `../Makefile` sets `SUB_DIRECTORIES = src include examples doc`, so top-level `make docs` / `make release_docs` recurse into `doc/`.
- `make/doc.mk` builds HTML with ExDoc from the compiled `../ebin/*.beam`, the app's markdown (`guides/*.md`, `notes.md`) plus `docs.exs`; it also renders man3 pages from the `.erl` files under `../src/` (selected via `grep -L "moduledoc false."`).
- The `mnesia` module Reference Manual is produced from `../src/mnesia.erl`; `release_docs_spec` installs the HTML into `$(RELEASE_PATH)/lib/mnesia-$(VSN)/doc/html` and man pages into `$(RELEASE_PATH)/man`.
- No legacy XML/SGML doc sources remain — guides are plain markdown, and the reference manual comes from in-source doc attributes.

## Constraints
- `doc/src/*.erl` are example programs, not doc-attribute sources; the guide embeds their code by hand, so keep the two manually in sync.
- Guide ordering/set is defined solely by `extras:` in `docs.exs` — add or reorder a chapter there.
- Do not break the doc examples: they are compiled with `-Werror` and executed by tests (see Notes for Agents).
- `misc/` is internal and is not part of the published documentation.

## Notes for Agents
- Test coupling is real: `../test/Makefile` compiles `../doc/src/{company,company_o,bup}.erl` (as `DocExampleModules`, with `-Werror`) and `mnesia_examples_test.erl` runs them (`bup/1`, `company/1`). A broken or renamed doc example fails the test suite even though it "is only docs".
- `mnesia_SUITE:doctests/1` runs `ct_doctest:module(mnesia, ...)`, i.e. it executes the code fences/moduledoc inside `../src/mnesia.erl` — not the files in `doc/src/`.
- The `%0`/`%1`/`%20` markers in `doc/src/*.erl`/`*.hrl` and `company.fig` are vestiges of an old figure-based doc tool; they are inert (no build effect, harmless).
- `doc/misc/Makefile` is dead/legacy: it references a nonexistent `OTP_MAKE_ROOT=/home/super/otp/otp_make` and an absent `release_notes.sgml`, and is included by nothing. The real doc build does not use it.
- `doc/src/` files ship as part of `mnesia_test` (installed by `../test/Makefile` `release_tests_spec`), not by `doc/Makefile`.

## See Also
- `../src/` — mnesia modules; `src/mnesia.erl` is the reference-manual doc source.
- `../test/` — `mnesia_SUITE` (`doctests` case) and `mnesia_examples_test` (runs the doc examples).
- `../examples/` — separate, larger example programs (`mnesia_meter`, `mnesia_tpcb`), distinct from `doc/src/`.
- `../../../make/doc.mk` — shared doc build rules; `../CONTEXT.md` — lib app conventions.
