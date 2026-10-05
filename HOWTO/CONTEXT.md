# HOWTO — Build, Test and Contribution Guides

## Intent
Human-facing Markdown howto documents for building, testing, cross-compiling, releasing and contributing to Erlang/OTP.
These are prose guides, not code: they describe procedures and prescribe commands for the top-level build system (`configure`, `otp_build`, `make`) and its helper directories (`make/`, `scripts/`, `xcomp/`, `.github/`).
Reference/manual documentation for the *product* lives in `../system/` and the per-app `doc/` directories; this directory holds the *project's own* developer documentation.

## Constraints
- Pure Markdown, no code. Editing a HOWTO does not require a rebuild, but `./otp_build check` validates cross-links after a doc build.
- Every file carries the standard SPDX Apache-2.0 `%CopyrightBegin%` header described in `FILE-HEADERS.md`; the `..` → `./scripts/license-header.es` scanner enforces the exact layout and auto-updates the copyright year, so never hand-edit the year.
- Files contain release-time placeholders (`%OTP-REL%`, `%OTP-VSN%`, `%EX_DOC_VSN%`) that are intentionally *not* substituted in the git tree.
- Links between the docs are relative (`TESTING.md`, `DEVELOPMENT.md`, `../CONTRIBUTING.md`) and are rewritten/normalized when the docs are published; keep them relative, not absolute GitHub URLs, except where the doc already uses absolute URLs.

## API Surface (documents, grouped by concern)

### Build & install
- `INSTALL.md` — canonical native build/install guide: required/optional utilities, unpack → `./configure` → `make` → `make release_tests` smoke test → `make install`, advanced `configure` options, `DESTDIR`/`EXTRA_PREFIX`/`RELEASE_ROOT` install modes, build types (`TYPE=debug|valgrind|asan|lcnt|gcov|gprof`), and the list of tested architectures/OSes. Linked from `../README.md`.
- `INSTALL-CROSS.md` — cross-compiling with `configure --host/--build` or `otp_build configure --xcomp-conf=<FILE>`; cross configuration variables, releasing/installing tests for the target, patch-submission checklist for new xcomp variables.
- `INSTALL-WIN32.md` — current Windows build path: WSL1+Ubuntu as the build shell, MSVC (`cl.exe`) as the C compiler, `otp_build env_win32 x64` → `configure` → `boot -a` → `release -a` → `installer_win32`; includes an FAQ explaining the VC++/mingw history.
- `INSTALL-WIN32-OLD.md` — legacy Cygwin/MSYS/MSYS2 build path; explicitly marked as no longer tested by the OTP team and slated for removal.
- `INSTALL-ANDROID.md` — cross-compile to Android via NDK using `xcomp/erl-xcomp-arm64-android.conf`; documents target deployment (Raspberry Pi/Android) and known `inet:gethostbyname/1` issue.
- `INSTALL-IOS.md` — cross-compile to iOS using `xcomp/erl-xcomp-arm64-ios.conf`; `RELEASE_LIBBEAM=yes` produces `libbeam.a` for Xcode `.xcframework` packaging, plus how to boot via `erl_start()`.
- `INSTALL-RASPBERRYPI3.md` — long-form walkthrough for building a crosstool-ng toolchain + sysroot on macOS and cross-compiling OTP to Raspberry Pi 3.
- `BOOTSTRAP.md` — why prebuilt `.beam` files are committed: the primary bootstrap (`bootstrap/{lib,bin}`) and preloaded code (`erts/preloaded`), and how/when they are regenerated.

### Testing & tooling
- `TESTING.md` — Common Test / `ts` workflow: `make test`, `make <app>_test ARGS="-suite X -case Y"`, `./otp_build tests` releasing to `$TESTROOT`, `ts:install()`/`ts:run()`/`ts:help()`, log locations, and running tests under ASan (`TS_RUN_EMU=asan`, `ASAN_LOG_DIR`) or Valgrind (`TS_RUN_EMU=valgrind`, `VALGRIND_LOG_DIR`). Linked from `../CONTRIBUTING.md`.
- `DEVELOPMENT.md` — day-to-day development guide and the best single entry point for working in this repo: `./otp_build configure`, small/tiny/all builds, per-app `make`/`test`/`dialyzer`/`docs`, `TYPE`/`FLAVOR` builds, `cerl` debugging switches, `./otp_build check`, doc build & link validation, GitHub Actions usage, Docker/Gitpod/devcontainer. Linked from `../CONTRIBUTING.md`.
- `BENCHMARKS.md` — benchmarks are CT testcases driven by the same `ts` framework; `ts:benchmarks()`, `ts:bench()`, results via `ct_event` `benchmark_data`, gated by `AppName_bench.spec` in `lib/*/test/`.
- `DTRACE.md` — DTrace probe support: `./configure --with-dynamic-trace=dtrace`, probe specs in `../erts/emulator/beam/erlang_dtrace.d`, examples in `../lib/runtime_tools/examples/`.
- `SYSTEMTAP.md` — the Linux/SystemTap counterpart: `./configure --with-dynamic-trace=systemtap`, kernel UTRACE and SystemTap version requirements, `stap -L 'process("beam").mark("*")'`.

### Process & meta
- `DOCUMENTATION.md` — how OTP documentation is structured and written (ExDoc + earmark_parser Markdown flavor, modules vs "extra pages", grouping, style rules, mermaid/diagrams, `since` annotation conventions). Points at `../make/ex_doc.exs`, per-app `docs.exs`, and `../system/doc/reference_manual/documentation.md` for the normative style guide.
- `FILE-HEADERS.md` — mandatory license header template, allowed copyright statement forms, short files (<20 lines), `.license` sidecar files for binaries, vendored-dependency (REUSE) rules; enforced by `../scripts/license-header.es`. Linked from `../CONTRIBUTING.md`.
- `RELEASE-NOTES.md` — guidance for writing user-visible release notes (don't reuse commit messages, prefer examples/links, reference PRs and issues, structure bug-fix notes as Symptom/Trigger/Version). Linked from `../CONTRIBUTING.md`.
- `DEPRECATE.md` — how to deprecate/remove APIs: `-deprecated`/`-removed`/`-deprecated_type`/`-removed_type` attributes with `Name/Arity` or `_` wildcards, update `../system/doc/general_info/DEPRECATIONS`, then run `./otp_build update_deprecations` (regenerates docs and `../lib/stdlib/src/otp_internal.erl`).
- `OTP-PATCH-APPLY.md` — patching an installed OTP with newer app versions using the top-level `../otp_patch_apply` tool (`-s`, `-i`, `-l`, `-c`, `-f`, `-n`), including the `system_information:sanity_check()` verification step.
- `SBOM.md` — source SBOM generation and maintenance: oss-review-toolkit (ORT) local/Docker flows, `.ort/config/config.yml`/`.ort.yml`, SPDX package/vendor handling via `vendor.info` files and `../.github/scripts/otp-compliance.es sbom`, and the OpenVEX workflow (`openvex.table`, orphan branch `openvex`, `vexctl`).

## Key commands for agents (build / test / check)
- Configure & full build: `export ERL_TOP=$(pwd); ./otp_build configure && make` (or `./configure && make`; `./otp_build boot -t|-a` for tiny/all).
- Rebuild one application: `cd lib/<app> && make`.
- Run tests: `make test` (all), `make <app>_test`, `make <app>_test ARGS="-suite <SUITE> -case <case>"`; emulator suite is the pseudo-app `emulator`.
- Build docs: `make docs` (needs `ex_doc` in PATH, or `./otp_build download_ex_doc`); `make docs DOC_TARGETS="html"`.
- Static analysis + doc link/`since` validation: `./otp_build check` (also runs Dialyzer; needs Elixir ≥ 1.16 for the link checker).
- License header check/fix: `./scripts/license-header.es scan --path <file>`.
- Regenerate derived artifacts after touching preloaded/bootstrap or deprecations: `./otp_build update_preloaded`, `./otp_build update_primary`, `./otp_build update_deprecations`.
- `MAKEFLAGS=-jN` and `ERLC_USE_SERVER=true` both materially speed up builds.

## Notes for Agents
- Do NOT edit `configure`/`Makefile` to fix issues described here; these docs only *describe* the build. The `.src`/`.in` sources live at the repo root and in `../make/`.
- The `%OTP-REL%` / `%OTP-VSN%` / `%EX_DOC_VSN%` tokens in `INSTALL.md`, `INSTALL-CROSS.md`, `INSTALL-WIN32.md`, `INSTALL-WIN32-OLD.md` are deliberate placeholders substituted when the docs are built: `../system/doc/Makefile` (target `installation_guide/%.md`) sed-replaces them while copying the HOWTOs into the installation guide, using `OTP_VERSION` and `make/ex_doc_vsn` (currently `v0.40.4`) as the values. They are not a defect — don't "complete" them.
- `../TAR.include` lists `HOWTO/MARKDOWN.md`, which does not exist in the tree (it was removed when docs moved to ExDoc). The file is only referenced by `../scripts/license-header.es`'s ignore list, so no in-repo build breaks; flagged here because it is how an outsider notices the stale entry.
- `DEVELOPMENT.md` explicitly warns that the non-`test`/non-`docs` make targets it showcases are *internal and unsupported* (no stability guarantee); treat only `INSTALL.md`/`TESTING.md` targets as the supported surface.
- `INSTALL-WIN32-OLD.md` documents an environment the OTP team no longer tests; prefer `INSTALL-WIN32.md` and treat the OLD file as historical reference only.

## Dependencies
- Documents describe external tooling rather than depending on it: GNU make/autoconf, gcc/clang, MSVC+WSL1 (Windows), Xcode (iOS), Android NDK, crosstool-ng (Raspberry Pi), OpenSSL, wxWidgets, JDK (`jinterface`), `ex_doc` (+ Elixir ≥ 1.16 for the link checker), valgrind, clang ASan, DTrace/SystemTap, Go + `vexctl` and Java/Gradle + ORT/scancode (SBOM/VEX).

## See Also
- `../CONTRIBUTING.md` — contribution workflow; links into `FILE-HEADERS.md`, `TESTING.md`, `DEVELOPMENT.md`, `RELEASE-NOTES.md` here.
- `../README.md` — points at `HOWTO/INSTALL.md` as the full build/install reference.
- `../system/doc/` — the system documentation *content* and the Makefile that copies/substitutes the HOWTOs into the installation guide.
- `../.github/` — CI workflows and `.github/scripts/otp-compliance.es` referenced by `SBOM.md` and `DEVELOPMENT.md`.
- `../make/` — `doc.mk`/`ex_doc.exs`/`otp.mk.in` implementing the `docs`/`test` targets described here (see `docs.exs` in each app's `doc/`).

## Status
Complete — all 19 Markdown files reviewed; no subdirectories, no code, no delegation needed.
