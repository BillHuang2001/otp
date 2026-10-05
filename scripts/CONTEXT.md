# OTP `scripts/` — Release Packaging, Compliance, Docs Validation & Static Analysis

## Intent
Flat directory of 10 standalone, executable helper scripts (no subdirectories, no build rules, nothing here is compiled or installed into the release).
They are the human- and CI-facing tooling around the build: packaging release tarballs, enforcing license/copyright headers, auditing and validating documentation, running static analysis, guarding pushes, and post-processing valgrind logs.
None of them are prerequisites of `make`/`make install`; they are entered via `otp_build`, top-level make targets, a git hook, or GitHub Actions.

## API Surface

### Release packaging
- `build-otp-tar` (bash) — creates the two OTP source archives from a source tarball: `SRC_CLN.tar.gz` (clean source) and `SRC_PREBLD.tar.gz` (source + prebuilt artifacts, default names, override with `-o`).
  Without `-b|--build-dir <fully built tree>` it builds the tree itself: `./otp_build configure --disable-hybrid-heap`, `./otp_build boot -a`, `make docs DOC_TARGETS=chunks`.
  It then (a) restores `@TARGET@ config.status config.log core core.*` (plus every `SKIP` file) to source state, (b) applies the `prebuild.skip` (restore listed objects), `prebuild.delete` (remove listed objects) and `prebuild.keep` (copy listed objects from the build tree) files, (c) writes `<tree>/prebuilt.files` — files present prebuilt but absent from the source tree — and optional `-d`/`-n` deleted/new-file logs.
  Other options: `-g|--gtar`, `-l|--build-log`, `-r|--remove-working-dir`, `-s|--silent`, `-w|--working-dir`, `-v`, `-h`.
  `prebuilt.files` is consumed later by `otp_build remove_prebuilt_files`.
- `bundle-otp` (bash) — legacy OTP-20-era bundler: takes a git tag, clones `erlang/otp` and `erlang/corba` at the matching tag into `$ERL_TOP/bundle/`, strips `.git`, records `repo tag sha` lines in `artifacts/<tag>.0-bundle.txt` and tars the trees into `artifacts/<tag>.0-bundle.tar.gz`.
  Not referenced by make, `otp_build`, or CI.

### License / copyright compliance
- `license-header.es` (escript, `argparse`-based CLI `license-header`) — validates and rewrites OTP's `%CopyrightBegin% … %CopyrightEnd%` blocks.
  Commands: `scan` (report), `ci` (repo-wide scan as CI runs it), `update` (= `scan --update`, rewrites Ericsson copyright year ranges).
  Options: `-v|--verbose` (shows template diffs), `-u|--update`, `--no-missing <globs>` (suppress "header not found" warnings), `--path <files|dirs|stdin>` (defaults to `$ERL_TOP` or CWD; `stdin` reads a file list, one per line).
  Checks the comment prefix, that the SPDX id has a matching `<id>.txt` template in `LICENSES/` (preferred) or `FILE-HEADERS/`, that copyright lines start with `Copyright ` or `SPDX-FileCopyrightText:`, that Ericsson lines match `Copyright Ericsson AB <years>. All Rights Reserved.`, and whether the file actually needs the full license text inline (short-file and `*_SUITE_data` heuristics; `<file>.license` sidecars are used for files without an inline header).
  The file set comes from `git ls-tree -r -z --name-only HEAD <dir>` (tracked regular files only); git history (`git log --author=@erlang.org/@ericsson.com`, custom rename following via `git diff-tree -M`) supplies the copyright years.
  Exits `1` when any warning was produced.

### Documentation audit & validation
- `otp_check_html_links.exs` (Elixir) — validates built HTML docs: duplicate `id`s, `<a>` without `href`, empty `href`, unresolved `a[href]` targets (files and `#anchors`), closest-match anchor suggestions (Jaro distance), and missing `img[src]` files.
  Positional argv = glob patterns; with no arguments it uses `doc/ doc/system/ lib/*/doc/html/ erts*/doc/html/ system/doc/html/`.
  `Mix.install([:floki])` fetches Floki at startup; exit status is the number of warnings, so any warning fails CI.
- `check_doc_since` (perl `-w`) — finds missing or wrong `since=` attributes in XML/edoc documentation by walking git tags newest→oldest and detecting documented modules/functions that "appear" between two tags.
  Tags come from STDIN (one per line, newest first) or `-t <new-tag> <old-tag>`; `-s` skips non-release tags, `-m <module>` limits to one module, `-v` also dumps "always existed" modules/functions.
  Prints `MODULE: <m> since=…` / `FUNC: <m:f/a> since = <versions>`; correctness relies on the hardcoded `%suppressions`, `%ambiguous_0args` and `%complex_arglist` allowlists (used to interpret `foo(Arg1 [,Arg2])` and `[Foo]`-style ambiguous arglists) and on `fixver()` normalizing sloppy version strings.

### Static analysis
- `run-dialyzer` (bash) — the top-level `make dialyzer` implementation: builds one temporary PLT for `BASE_PLT` (`compiler crypto erts kernel stdlib syntax_tools`) plus `APP_PLT`, then dialyzes with `-Wunknown -Woverlapping_contract -Wopaque_union` in three buckets — `UNMATCHED` apps with `-Wunmatched_returns`, `NO_UNMATCHED` apps, and `WARNINGS` (`diameter`, whose failures are tolerated via `|| true`).
  Prefers `$ERL_TOP/bin/dialyzer` / `$ERL_TOP/bin/win32/dialyzer.exe`, else `dialyzer` from PATH; the legacy `TRAVIS_SKIP` env var removes apps from every bucket.
- `otp_build_check` (sh) — the umbrella "is my branch ready for daily builds?" driver, invoked as `./otp_build check` (also directly). Steps, each skippable via flags: configure (only if `make/config.status` is absent), optimized `make` + `cerl` boot, debug build + `cerl -debug` boot (`--only-opt|-o` skips), `make format-check` (needs `clang-format`; `--no-format-check|-f`), `make release` + `Install -minimal` + boot of `bin/erl`, `make docs`/`release_docs` + `scripts/otp_check_html_links.exs` (`--no-docs|-d`), `make dialyzer` (`--no-dialyzer|-y`), `scripts/license-header.es ci` (`--no-license-header|-l`), and releasing+compiling all test suites via `otp_build tests` / `make release_tests` (`--no-tests|-n`, `--tests|-t <apps>`).
  Requires `ERL_TOP` set to the source root containing `OTP_VERSION`; `CONFIG_FLAGS` is forwarded to `configure`.
- `valgrind_beamasm_update.escript` (escript) — invoked with `<valgrind log file> <perf symbol map file>`; replaces unresolved JIT frames in the valgrind log with `Name+offset` from the emulator's perf symbol map (`0xADDR SIZE $global::name`), handling both plain (`by|at 0xADDR: ???`) and XML (`<ip>0xADDR</ip>`) valgrind output, and rewrites the log in place.
  Only reachable from `erts/etc/unix/cerl.src` when running `cerl -valgrind` with `VALGRIND_LOG_DIR` set and `ERL_SYM_MAP_FILE` present (emulator name must be `beam.valgrind.smp`).
- `diffable` (escript, compiled) — compiles a fixed list of OTP applications (+ the Elixir stdlib if an `elixir` checkout sits next to `ERL_TOP`) to diff-friendly `.S` (default) or `.dis` files so `diff -u old new` shows the effect of a compiler/loader change.
  Options: `--asm`, `--legacy-asm` (Latin-1 output), `--dis`, `--no-compile` (disassemble existing BEAMs), `--deterministic`, `--co <compiler option>`, `--erltop <path>` (compile another tree), `--time`.
  Requires a *built* tree: it relies on the compiler option `diffable` (`lib/compiler/src/compile.erl`) and `erts_debug:disassemble/1`, and renumbers labels/hashes so output is stable across runs.

### Push policy
- `pre-push` (sh) — the OTP git pre-push hook; opt-in by copying it to `.git/hooks/pre-push` (the script documents this in its header, and the hook itself tells users to refresh it from `refs/remotes/<remote>/master:scripts/pre-push`).
  Only acts for `github.com/erlang/otp.git` (any other remote ⇒ "No checks done"). It forbids: pushing to an unnamed remote, deleting or renaming refs, force-updates, branches outside `master`/`maint`/`maint-NN`/`maint-rNN`, unknown `OTP-*` tags or tags that already exist, pushes with more than 100 commits or 100 changed files, and commit messages matching `fixup!|squash!|TEST|DEBUG`.
  It requires `maint` to be an ancestor of `master` (and warns when `maint-NN` is not merged forward), forbids the `MASTER_ONLY` first-master commit from being reachable from maint branches, and refuses to push when the installed hook is older than the remote copy (`PRE_PUSH_SCRIPT_VERSION=8`).

## Invocation map
- `make dialyzer` (top level, `Makefile.in`, only when `TYPE` is empty) → `scripts/run-dialyzer`.
- `./otp_build check` (`otp_build:1044` `exec $ERL_TOP/scripts/otp_build_check`) → `otp_build_check`, which in turn calls `make format-check`, `make dialyzer`, `scripts/otp_check_html_links.exs`, `scripts/license-header.es ci`, `otp_build tests`.
- `cerl -valgrind` with `VALGRIND_LOG_DIR` (`erts/etc/unix/cerl.src`) → `valgrind_beamasm_update.escript`.
- CI: `.github/workflows/license-scanner.yaml` → `license-header.es ci`, plus an `update`-then-`ci` idempotence check (job pins OTP 27); `.github/workflows/reusable-documentation.yaml` → `otp_check_html_links.exs` inside the released `docs` dir; `.github/workflows/reusable-static-analysis.yaml` → `make format-check` and `run-dialyzer`; `.github/workflows/reusable-pack.yaml` and `.github/scripts/sync-github-releases.sh` → `build-otp-tar` (the source tarball it consumes is produced by `.github/scripts/init-pre-release.sh`).
- Developer-only, not wired into make or CI: `diffable`, `check_doc_since`, `bundle-otp`, `pre-push`.

## Relationship to the `prebuild.*` mechanism
- `scripts` is listed in the repo-root `prebuild.delete`, so this directory is stripped from the *pre-built* archive produced by `build-otp-tar` while remaining in the clean source archive.
- Root `prebuild.skip` (`Makefile bin bootstrap/bin ibin`) is the file that "restores to source state"; do not add `scripts` there — the intent is that this tooling is not shipped in the pre-built tar.

## Constraints
- Every file is executable with a shebang; keep the mode (CI and `otp_build` rely on direct execution).
- Scripts must work from either a source checkout or an installed release: resolve paths from `ERL_TOP`/CWD, never hardcode absolute paths.
- This directory is itself subject to the rules it enforces: each script carries a `%CopyrightBegin%` block checked by `license-header.es`; keep those headers and the `REUSE-IgnoreStart/End` markers in `.es`/`.escript` files intact.
- Language split is deliberate: sh/bash for build+git plumbing, perl for the tag-diffing regex work, escript for OTP-data tasks, Elixir only where an HTML parser (Floki) is required.
- Do not add scripts that duplicate existing make targets — extend the invoked target instead.

## Dependencies
- Erlang/OTP `escript`: `argparse` (OTP 26+) and the stdlib `json` module (OTP 27+) are required by `license-header.es` — its CI job pins OTP 27; `valgrind_beamasm_update.escript` uses `ets`/`re`; `diffable` needs a built tree of the same checkout.
- Elixir + Hex (to fetch `floki`) for `otp_check_html_links.exs`; the documentation workflow runs it inside a docker image.
- perl with `File::Basename` for `check_doc_since`.
- GNU tar (`-i -B -p` used by `build-otp-tar`) and GNU grep (`-P`)/`sort -V` by `bundle-otp`.
- `clang-format` for the `otp_build_check` format step; `valgrind` plus the emulator perf symbol map for `valgrind_beamasm_update.escript`.
- git everywhere: `check_doc_since` checks out tags, `license-header.es` uses `ls-tree`/`log`/`diff-tree`, `pre-push` inspects remote-tracking refs.

## Known Issues / Gotchas
- `check_doc_since` runs `git checkout -f <tag>` in the repo it is invoked from, so it leaves a detached HEAD and destroys uncommitted work — never run it in a dirty worktree.
- `build-otp-tar` runs `cd $start_root` before packing, but `start_root` is never assigned (`start_dir` is the variable it sets); `cd` with an empty argument goes to `$HOME`, and the archive paths happen to be absolute so the result is still correct.
- `build-otp-tar` has other latent bugs in rarely-taken branches: `tmp_dir=/tmp` ignores `$TMPDIR`, and the `/tmp`-missing fallback `$tmp_dir=$start_dir` is not a valid assignment.
- `build-otp-tar` does not use `set -e`; it relies on its own `error`/`warning` helpers and a `remove()` safety check that refuses paths outside `$work_dir`.
- `license-header.es` `ci` compares new files against the hardcoded tag `OTP-27.3` and carries long allowlists (`NoWarnNewFiles`, `NoWarnAllFiles`) that the source comments describe as things "we currently ignore… all of these should be fixed".
- `license-header.es` only sees files tracked by git in `HEAD`, and skips `*.license` and `mix.lock`; a file added but not committed will not be scanned.
- `otp_check_html_links.exs` exits with the warning count, and its `if false do … "Unused anchor" …` block is dead code (unused-anchor checking is disabled).
- `pre-push` requires an up-to-date `refs/remotes/<remote>/master`; it refuses pushes when the installed hook differs from the remote copy, so bumping `PRE_PUSH_SCRIPT_VERSION` forces users to reinstall the hook.
- `bundle-otp` does `mkdir bundle` (no `-p`) and would fail on a second run; it is unexercised by CI and may be obsolete.
- `run-dialyzer` keeps a `TRAVIS_SKIP` escape hatch and hardcoded per-app buckets; adding a new app that should not be dialyzed requires editing the bucket lists here.

## Notes for Agents
- Nothing here is generated; all 10 files are hand-written sources and safe to edit in place.
- `scripts/` is a leaf directory — there are no child nodes to delegate to.
- `configure`/`Makefile` are generated elsewhere in the repo; `build-otp-tar` and `otp_build_check` only *invoke* `./otp_build configure|boot`, they do not contain configure logic.
- To exercise anything here you need a fully built OTP; the fastest broad smoke test is `./otp_build check` (or targeted steps such as `./scripts/license-header.es scan --path <dir>` and `make dialyzer`).

## Test Strategy
- There is no unit-test suite for this directory; correctness is enforced by CI: the license scanner workflow (including an `update`+`ci` idempotence check), reusable-static-analysis (`format-check` + `run-dialyzer`), reusable-documentation (HTML link check), and reusable-pack (`build-otp-tar`).
- Local verification: `./otp_build check` for the umbrella run; `./scripts/license-header.es scan --path <file|dir>` for a fast header check; `./scripts/otp_check_html_links.exs <glob>` after `make release_docs`.

## See Also
- `../HOWTO/DEVELOPMENT.md` — "Static analysis" and "Validating documentation" sections describe `otp_build check` and the HTML link-check output format.
- `../HOWTO/FILE-HEADERS.md` — authoritative license-header rules; documents `license-header.es scan`.
- `../LICENSES/`, `../FILE-HEADERS/` — SPDX templates resolved by `license-header.es` (FILE-HEADERS wins on duplicates).
- `../otp_build` — `check`, `remove_prebuilt_files`, and the `download_ex_doc` target needed for `make docs`.
- `../Makefile.in` — `dialyzer`, `format-check`, `docs`/`release_docs`, `install-docs`, and the `prebuilt.*` handling used with `build-otp-tar`.

## Status
Complete: all 10 scripts documented, invocation sites identified, no subdirectories to delegate.

## Routing Table
`scripts/` is flat (10 executable files, no subdirectories) — no delegation targets inside this node. Cross-references (read-only; escalate writes to the parent):
- `../erts/` → emulator; `erts/etc/unix/cerl.src` calls `scripts/valgrind_beamasm_update.escript` (sibling — read-only, escalate writes to parent)
- `../lib/compiler/` → the `diffable` compile option/pass consumed by `scripts/diffable`
- `../make/` → shared make infrastructure, `format-check`, ex_doc wrapper used by the doc build
- `../.github/` → CI workflows and scripts that invoke `build-otp-tar`, `license-header.es`, `run-dialyzer`, `otp_check_html_links.exs`
- `../system/` → documentation sources whose generated HTML is validated by `otp_check_html_links.exs`
