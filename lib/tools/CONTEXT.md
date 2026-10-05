# tools — OTP Development Tools Application

## Intent
The `tools` OTP application (`lib/tools/`) ships the development-time/analysis toolset of Erlang/OTP: coverage, cross-reference analysis, profilers, TAGS generation, Make/Emakefile, crash-dump decryption, and the Emacs Erlang mode.
It is a normal in-tree application with a flat `src/` (one directory, no `include/`, no `c_src/`, no per-app `configure`) and is built by default (`ERLANG_APPLICATIONS` list in `../Makefile`).
Version: `TOOLS_VSN = 4.2.3` in `vsn.mk` (generated `tools.app` gets it substituted for `%VSN%`); the Emacs mode version is separate (`TOOLS_VSN` vs `EMACS_VSN` in `emacs/vsn.mk`).

## API Surface
`Makefile` — `SUB_DIRECTORIES = src doc examples emacs`; sets `VSN = $(TOOLS_VSN)`, `DIA_PLT_APPS = compiler runtime_tools`, `TEST_NEEDS_RELEASE = true`; includes `make/app_targets.mk`.
`src/tools.app.src` — application resource: `applications = [kernel, stdlib]`, `registered = []`, env `file_util_search_methods`, and `runtime_dependencies` naming `stdlib`, `runtime_tools`, `kernel`, `erts`, `compiler`, `public_key`, `crypto`.
`src/tools.appup.src` — appup with a single `{<<".*">>, [{restart_application, tools}]}` entry for both upgrade and downgrade.
Public tool modules (`src/`): `cover` (line coverage), `xref` (cross-reference API), `cprof`/`eprof`/`fprof`/`tprof`/`lcnt` (profilers), `crashdump` (encrypted crash dumps), `make` (Emakefile), `tags` (Emacs TAGS generator).
`xref` is implemented by the private `xref_base`/`xref_compiler`/`xref_reader`/`xref_scanner`/`xref_utils` plus the Yecc-generated `xref_parser` (from `xref_parser.yrl`) — only `xref` is public API.
`examples/xref_examples.erl` — a runnable example script showing `xref` usage; installed to `$(RELEASE_PATH)/lib/tools-$(VSN)/doc/examples`.
`priv/styles.css` — stylesheet used by `cover`'s HTML output; installed to `$(RELEASE_PATH)/lib/tools-$(VSN)/priv`.

## Constraints
- Public documentation is the edoc/ExDoc Markdown inside the `-moduledoc`/`-doc` attributes of `src/*.erl`; `doc/src/` no longer holds per-module doc sources (only `venn1.fig`/`venn2.fig` figures).
- Never hand-edit generated files: `src/xref_parser.erl` (from `xref_parser.yrl`), `ebin/tools.app`, `ebin/tools.appup`.
- App version changes go in `vsn.mk` only; `%VSN%` substitution happens in `src/Makefile`.
- `src/Makefile` `MODULES` must list every module installed into `ebin`; sources compile with `-Werror +nowarn_deprecated_catch` (warnings are fatal).

## Dependencies
- Runtime `applications`: `kernel`, `stdlib` only (that is what is started with the app).
- `runtime_dependencies` (declared, not started): `runtime_tools`, `compiler`, `public_key`, `crypto`, `erts`.
- Build/test extras: `DIA_PLT_APPS = compiler runtime_tools` (Dialyzer PLT) and `TEST_NEEDS_RELEASE = true` (tests run against a released OTP, not the in-tree `ebin`).
- `emacs/` requires an Emacs binary to produce/verify `.elc`; `emacs_SUITE` needs Emacs >= 27.1 (skips otherwise).

## Notes for Agents
- `lc`, `erl_tidy` and `instrument` are NOT part of this application: `instrument` lives in `../runtime_tools/`, and `lc`/`erl_tidy` no longer exist under `lib/`. Do not look for their CONTEXT here.
- `cover` is by far the largest module (~3100 lines) and `xref` is spread over 7 modules; both have dedicated large test suites.
- Tooling-level requirements that explain failures: `xref` needs `debug_info` in the analyzed BEAM; `cover` needs cover-compiled modules or native (OTP 27+) coverage; the profilers need a live node; `lcnt` needs a lock-counting emulator; `crashdump` needs a dump from an `--enable-encrypted-crash-dumps` build.
- `tprof` is experimental and is intended to replace `cprof`/`eprof`.

## Test Strategy
- `make test` in this directory releases Erlang to a temp dir (because `TEST_NEEDS_RELEASE=true`) and runs `ct_run -spec ../tools_test/tools.spec`; the released `tools_test` dir holds the suites.
- Suites: `tools_SUITE` (app/appup sanity), `cover_SUITE` and `xref_SUITE` (the two heavyweight suites), `fprof_SUITE`, `eprof_SUITE`, `tprof_SUITE`, `cprof_SUITE`, `lcnt_SUITE`, `make_SUITE`, `tags_SUITE`, `crashdump_SUITE`, `emacs_SUITE`, `prof_bench_SUITE` (benchmark, run via `tools_bench.spec`, excluded from `tools.spec`).
- `test/tools.cover` configures CT self-coverage of the app.
- Known gaps: `prof_bench_SUITE` and `emacs_SUITE` silently skip when their prerequisites (free disk / `emacs` on `PATH`) are missing, so a green run may mean "not measured".

## Routing Table
- `src/` → all tool modules (`cover`, `xref`+internals, profilers, `crashdump`, `make`, `tags`, app resources)
- `test/` → Common Test suites and `*_SUITE_data/` fixtures for every tool
- `doc/` → ExDoc config `docs.exs`, `guides/` chapters, `references/erlang.el.md`, release notes `notes.md`, `assets/`+`src/` figures
- `emacs/` → Erlang Emacs mode (`erlang.el` and friends), its README, internal SGML doc, `tags` man page
- `examples/` → `xref_examples.erl` usage example (installed as documentation)
- `priv/` → `styles.css` for `cover` HTML reports
- `ebin/` → build output only (`.gitignore` tracked); never edit
- `../runtime_tools/` → `instrument`, `dyntrace`, `system_information` (sibling — read-only, escalate writes to parent)
- `../common_test/test_server/` → `ts_install_cth`/test server used by the suites (sibling — read-only)

## See Also
- `../CONTEXT.md` — OTP library applications overview and build-order rules.
- `../../make/doc.mk`, `../../make/app_targets.mk`, `../../make/test_target_script.sh` — the shared build/test machinery this app hooks into.
