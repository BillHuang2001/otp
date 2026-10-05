# Erlang Mode for Emacs

## Intent
Emacs Lisp implementation of the Erlang major mode and its companion libraries, shipped as part of the `tools` application.
Sources live here; at release time the `.el` files + `README` are installed into `$(RELEASE_PATH)/lib/tools-$(TOOLS_VSN)/emacs` for users to add to their Emacs `load-path`.
Everything else is plain Emacs Lisp with no build/autoconf step — there is no `configure` here (unlike `common_test`/`crypto`/`wx`/... apps).

## API Surface
The `.el` files and their roles:
- `erlang.el` (5941 lines) — the main library, provides `'erlang`: `erlang-mode` (derived from `prog-mode`), indentation, font-lock/syntax highlighting, `inferior-erlang` shell integration, compile, electric commands, TAGS integration. `Package-Requires: ((emacs "27.1"))`.
- `erlang-start.el` — the user entry point (`(require 'erlang-start)`): autoloads the mode's commands and registers `.erl`/`.hrl`/`escript` in `auto-mode-alist`/`interpreter-mode-alist`. Users should require this, not `erlang.el`.
- `erlang-skels.el` (1680 lines) — `erlang-skel` tempo skeletons/templates (module, function, headers, ...).
- `erlang-edoc.el` — `erlang-edoc-mode`, EDoc `@tag` comment highlighting/filling.
- `erlang-eunit.el` — run EUnit tests from Emacs.
- `erlang-flymake.el` — on-the-fly syntax checking via `flymake`, invoking `erlc`.
- `erldoc.el` — browse OTP HTML docs (`erldoc-browse`/`erldoc-browse-topic`/`erldoc-apropos`); requires Emacs `dom` and `json` libraries.
- `erlang-test.el` — ERT unit tests for `erlang.el` (requires Emacs 25+); run by the CT suite, not by users.
- `Makefile` — installs `EMACS_FILES` (`erldoc erlang-skels erlang-start erlang-eunit erlang-edoc erlang-flymake erlang-test erlang`) + `README` into `$(RELSYSDIR)/emacs`; `clean` removes byte-compiled `.elc` files.
- `vsn.mk` — `EMACS_VSN = 2.7.0`, the Emacs-mode's own version, independent of `TOOLS_VSN`; `Makefile` still uses `TOOLS_VSN` for `RELSYSDIR`.
- `README` — user setup notes (load-path, `erlang-root-dir`, `exec-path`, require `erlang-start`; optional flymake addon).
- `AUTHORS` — original authors and maintainers.
- `tags.erl` — git symlink to `../src/tags.erl` (the `tags` program that generates TAGS files). No Makefile here builds or installs it; the real module is compiled and installed by `../src/Makefile` (it is in `MODULES`), so do NOT edit through the symlink.
- `tags.3` — stale/legacy man page (old-style `ERLANG MODULE DEFINITION` format, dated 1996) for the `tags` tool. No Makefile in the tree installs it: this dir's `release_man_spec` is empty and `EMACS_FILES` excludes it, and `../src/Makefile` has no man-page rule. The tool itself lives in `../src/tags.erl` and its current documentation is generated from that module's `-moduledoc`/`-doc` into the app's `../doc/` output.

## Constraints
- `*.elc` byte-compiled files are build artifacts produced by the CT suite / manual byte-compilation — never commit or hand-edit them.
- `erlang-int-bifs` / `erlang-ext-bifs` in `erlang.el` embed the runtime BIF/interpreter list and must stay in sync with the actual runtime; the `bif_highlight` test enforces this.
- Edit docs elsewhere: the mode's user documentation is hand-maintained markdown in the sibling `../doc/` (`references/erlang.el.md`, `guides/erlang_mode_chapter.md`), not generated from these sources — see "See Also".
- `emacs/` is a build sub-directory of `tools` (listed in `../Makefile` `SUB_DIRECTORIES = src doc examples emacs`).

## Notes for Agents
- The published docs are NOT generated from the `.el` files; `../doc/references/erlang.el.md` and `../doc/guides/erlang_mode_chapter.md` are edited by hand (both listed in `../doc/docs.exs`).
- `erlang.el` is a single very large file and is conventionally kept that way — prefer targeted edits over splitting.
- `internal_doc/emacs.sgml` is a large internal (non-published) SGML/Stork document describing the mode; it is not built by any Makefile.

## Dependencies
- A GNU Emacs binary on `PATH` is needed to run the tests / byte-compile.
- Emacs 27.1+ for `erlang.el`; Emacs 25+ for `erlang-test.el`.

## Test Strategy
- Tested from the sibling test directory `../test/emacs_SUITE.erl` (with golden-file data in `../test/emacs_SUITE_data/`) — read-only; escalate writes to the parent `tools` node.
- `emacs_SUITE` cases: `bif_highlight` (mode's BIF lists vs. runtime), `load_interpreted`, `compile_and_load` (byte-compiles every `.el`, warnings-as-errors for a NoWarn allowlist), `indent` (indents `emacs_SUITE_data/*` and diffs against the originals), `tests_interpreted` / `tests_compiled` (runs the `erlang-test.el` ERT suite).
- The suite skips when no Emacs is found, and skips all but `bif_highlight` unless Emacs >= 27.1.

## Routing Table
- `internal_doc/` → internal (non-published) SGML/Stork user document for the mode.

## See Also
- `../doc/` → published user guide + reference manual for the mode (sibling — read-only, escalate writes to parent).
- `../test/` → Common Test suite `emacs_SUITE.erl` and its data (sibling — read-only).
- `../src/tags.erl` → the `tags` tool source that `emacs/tags.erl` symlinks to.
- `../CONTEXT.md` → the enclosing `tools` application.
