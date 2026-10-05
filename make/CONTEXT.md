# make/ — OTP Build-System Infrastructure

## Intent
Shared build-system layer for the whole Erlang/OTP monorepo.
Holds the makefile include fragments that every component Makefile (top-level, `erts/*`, `lib/*`) includes, the OTP-local autoconf macro library, and the cross-cutting build/release/doc helper scripts.
Everything here is shared: an edit in this directory changes the build of every OTP component.

## API Surface (makefile fragments included by component Makefiles)
- `otp.mk.in` → generated per-host `make/$(TARGET)/otp.mk` (by `erts/configure`). Base include for OTP components.
  Defines `OTP_VERSION`, `SYSTEM_VSN`, `OTP_RELEASE`, `CROSS_COMPILING`, `DEFAULT_TARGETS = opt debug release release_docs clean docs`, `TYPES = opt debug lcnt valgrind asan gcov`, `FLAVORS`/`PRIMARY_FLAVOR`/`USE_PGO`, tool vars (`CC`,`LD`,`AR`,`INSTALL*`,`PERL`), Erlang vars (`ERLC`,`ERL`,`ESCRIPT`,`ERL_COMPILE_FLAGS`), implicit rules for `.erl`→`.beam`, `.yrl`/`.xrl`→`.erl`, MIB, Java, Emacs; doc dirs `HTMLDIR`/`MAN{1,3,4,6,7}DIR`/`CHUNKSDIR`; `EX_DOC`.
- `output.mk.in` → generated `make/output.mk` (by `make/configure`). Silent-rules helper layer.
  Provides `$(V)` (default `@DEFAULT_VERBOSITY@` from `--enable-silent-rules`; `V=0` silent, `V=1` verbose), `V_at`, `V_colon`, and per-tool `<cmd>_verbose` / `V_<CMD>` prefixes (`V_CC`, `V_ERLC`, `V_LD`, `V_JAVAC`, `gen_verbose`, `snmp_verbose`, `vsn_verbose`, …). Use these prefixes in new recipes to keep output clean.
- `target.mk` — computes `$(TARGET)` (canonical host triple) via `autoconf/config.guess` + `config.sub`; honors `OVERRIDE_TARGET` (with warning); hard-errors if neither can be determined. Included first by nearly every Makefile.
- `otp_subdir.mk` — recursive subdirectory driver. Iterates `$(SUB_DIRECTORIES)`, skips a dir containing a `SKIP` file, propagates the default targets. On `release` it also appends `app-vsn` to `releases/$(SYSTEM_VSN)/installed_application_versions` (via `otp_released_app.mk`); on `clean` it runs `dclean` when a `vsn.mk` exists.
- `app_targets.mk` — per-application targets: `test` (delegates to `test_target_script.sh`), `dialyzer`/`dialyzer_plt` (PLT under `priv/plt/`), `dclean`, `gclean` (`git clean -fXd`), `info`. Derives `APPLICATION` from `$(PWD)` when unset.
- `doc.mk` — documentation targets `docs`/`html`/`chunks`/`man` and the `release_*_spec` install rules. HTML via `ex_doc_wrapper` + `ex_doc.exs` (per-app overrides in the app's `docs.exs`); man pages via `markdown_to_man.escript`. See "Doc build pipeline".
- `otp_release_targets.mk` + `otp_default_release_path.mk` — define the generic `release`/`release_docs`/`release_tests` targets: re-invoke make with `RELEASE_PATH=$(OTP_DEFAULT_RELEASE_PATH)` (or `$(TESTROOT)` when set) and target `$@_spec`. `OTP_DEFAULT_RELEASE_PATH = $(ERL_TOP)/release/$(TARGET)`.
- `otp_released_app.mk` — small standalone makefile invoked by `otp_subdir.mk` to update `installed_application_versions` in the release (or `TESTROOT`).
- `dep.mk` — optional (`include` after defining `ERL_FILES`/`HRL_FILES`) `deps` target generating `deps/deps.mk` from `erlc -M`, with paths rewritten relative to `$(DEP_REL_TOP)`; the generated file is `-include`d.
- `run_make.mk` — for architecture-dependent subdirectories: forwards targets to `$(MAKE) -f $(TARGET)/Makefile` (`TYPE=`/`FLAVOR=` for opt/debug/emu/jit).
- `otp_ded.mk.in` → generated `make/$(TARGET)/otp_ded.mk` (by `make/configure`). Variables for building Dynamic Erlang Drivers: `DED_CC`, `DED_LD`, `DED_CFLAGS`, `DED_LIBS`, `DED_EXT`, `PRIVDIR`/`OBJDIR`/`LIBDIR`, `DED_MK_VSN` (must stay explicit; read before configure runs).

## API Surface (configure)
- `make/configure.ac` → `make/configure` (generated; regenerate with `./otp_build update_configure`, never hand-edit either).
  Requires autoconf 2.72, GNU make, Perl 5, `env`; disables config caching by default; validates that `$ERL_TOP` is set and absolute; picks up `ERTS_VSN`, `OTP_REL`, `OTP_VSN`; sets `DEFAULT_VERBOSITY` from `--enable-silent-rules`.
  Outputs `make/output.mk` and `make/$host/otp_ded.mk`, and sets `TARGET`/`CROSS_COMPILING` for its own Makefiles.
  The top-level `configure` runs the sub-configures in this order: `make`, then `lib/*`, then `erts` (erts last, non-parallel-safe).

## API Surface (helper scripts)
- `install_bin` — installs release executables into the bin dir, honoring `DESTDIR`/`EXTRA_PREFIX`/`bindir`; portable shell, run on all non-Windows platforms. Called from the top-level Makefile.
- `install_dir_data.sh.in` → generated `make/install_dir_data.sh` (by `erts/configure`) — recursively install a directory tree with `INSTALL_DIR`/`INSTALL_DATA`.
- `make_emakefile.in` → generated `make/make_emakefile` (by `erts/configure`) — Perl script converting an `erlc` argument list into one `Emakefile` entry.
- `ex_doc_wrapper.in` → generated `make/ex_doc_wrapper` (by `erts/configure`) — runs ex_doc (or the version found on `PATH`), exports `EX_DOC_VERSION`, and forces `EX_DOC_WARNINGS_AS_ERRORS=false` when any `lib/*/ebin/*.app` is missing.
- `check_ex_doc` — invoked by `docs`/`release_docs` before running ex_doc; if ex_doc is missing it offers to download it via `otp_build download_ex_doc`.
- `get_ex_doc_formats_no_epub.sh` — prints `-f html -f markdown` (drops markdown output for ex_doc < 0.40).
- `ex_doc_link` / `ex_doc_vsn` / `ex_doc.sha1sum` / `ex_doc.sha256sum` — pinned ex_doc release URL, version and checksums used by `otp_build download_ex_doc`.
- `ex_doc.exs` — the shared ex_doc config file (apps may add a local `docs.exs` merged over it). Derives `groups_for_docs` from module docs, builds `source_url` links to GitHub, collects `extras` from `*.md`/`guides`/`references`/`internal_docs`. Asserts that the current directory is a `doc` directory.
- `fixup_doc_links.sh` — rewrites relative HTML links in released docs to `lib/<app>-<vsn>/doc/html` form (uses `otp_versions.table` + `vsn.mk`).
- `markdown_to_man.escript` — converts markdown docs (or `-moduledoc`) into man pages for sections 1/3/4/6/7.
- `test_target_script.sh` — drives the app-level `make test` target (ct_run invocation, warnings about long runs).
- `cross_check_erl` — verifies a cross-compiled `erl` actually runs on the target.
- `verify_runtime_dependencies` — verifies `runtime_dependencies` (and `installed_application_versions`) when patching OTP apps.
- `patch_apply_adjust_docs` — adjusts doc versions/links while applying an OTP patch set.
- `fixup_development_runtime_dependencies` — replaces unresolved `app-@ticket@` runtime deps in `.app` files with the current source-tree versions in maint/master dev trees.
- `save_args` — stores/compares an argument string into a file (`save_args <file> --- <args>`).
- `unexpected_use` — canary: errors out if some script resolves to it during the `install`/`release` phase.
- `otp_version_tickets`, `otp_version_tickets_in_merge`, `otp_patch_solve_forward_merge_version` — small data/tooling files used by `otp_patch_apply` and its merge helpers (`.license` files alongside).
- `gdb_tools_vsn` / `gdb_tools_link` — pinned commit hash and repo URL for `otp_build download_gdb_tools`.
- `prebuild.skip` — list of prebuilt paths to keep out of the bootstrap build (currently just `.`).

## Build-System Contract (variables child makefiles rely on)
- `ERL_TOP` — absolute OTP root; required.
- `TARGET` — canonical host triple; determines which `make/$(TARGET)/otp.mk` and `$otp_ded.mk` are used.
- `OTP_VERSION`, `OTP_RELEASE`, `SYSTEM_VSN` — from `make/$(TARGET)/otp.mk`.
- `VSN` (set from the app's `vsn.mk`, e.g. `VSN = $(KERNEL_VSN)`), `APPLICATION`, `APPLICATION_VSN`/`<APP>_VSN` — app-level metadata.
- `SUB_DIRECTORIES` — list consumed by `otp_subdir.mk`.
- `RELEASE_PATH`, `TESTROOT`, `OTP_DEFAULT_RELEASE_PATH` — release output locations.
- `TYPE`, `FLAVOR`, `BUILD_ALL`, `BOOTSTRAP`, `OTP_SMALL_BUILD`, `V` — build-shape switches.

## Adding a New OTP Application
1. Create `lib/<app>/` with `Makefile` (includes `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, local `vsn.mk`, `$(ERL_TOP)/make/otp_subdir.mk`, `$(ERL_TOP)/make/app_targets.mk`), plus `src/`, `doc/`, `vsn.mk`, `ebin/`.
2. Register it in `lib/Makefile` (`ERLANG_APPLICATIONS`); apps not listed are still picked up under `BUILD_ALL` via the `wildcard */` fallback (`EXTRA_APPLICATIONS`).
3. Add an entry to `otp_versions.table`.
4. Add a `configure.ac` if the app needs configure (the top-level `configure` auto-discovers `lib/*/configure`).

## Constraints
- Files ending in `.in` are autoconf templates (`@VAR@` placeholders). Never edit their generated outputs (`make/output.mk`, `make/$(TARGET)/otp.mk`, `make/$(TARGET)/otp_ded.mk`, `make/make_emakefile`, `make/ex_doc_wrapper`, `make/install_dir_data.sh`, `make/configure`); edit the template and run `./otp_build update_configure`.
- These fragments are included by every component Makefile: changes are tree-wide, so prefer adding new variables/targets over changing existing semantics.
- Keep recipes silent-rule aware by prefixing commands with the appropriate `<cmd>_verbose`/`V_<CMD>` variable.
- `doc.mk`'s `man7/%.7` rule assumes the MIB lives under `$(ERL_TOP)/lib/<app>/mibs/`; it derives the app name from the path.

## Design Decisions
- The `make/` directory is configured first (it only needs a target triple), so `output.mk` and `target.mk` are available before any app is configured.
- `otp.mk`/`otp_ded.mk` are generated per host triple under `make/$(TARGET)/` so that cross-builds and multi-target trees can coexist.
- `otp_subdir.mk` recurses with a shell loop over `SUB_DIRECTORIES` (not parallel `$(MAKE) -C`) — this centralizes SKIP handling and the release/clean special-casing.
- Documentation is built per application with ex_doc into each app's `doc/html`, then link-rewritten at release time; man pages are generated from the same markdown sources.

## Known Issues
- The final install/release steps must not invoke `unexpected_use`; it exists purely so a mis-wired tool name fails loudly.
- `ex_doc_wrapper` and `get_ex_doc_formats_no_epub.sh` degrade gracefully (fewer formats / warnings not errors) when ex_doc is absent — missing-doc errors can therefore be silent in such setups.

## Notes for Agents
- Generated/ignored files in this directory (see root `.gitignore`): `output.mk`, `ex_doc_wrapper`, `make_emakefile`, `install_dir_data.sh`, `config.log`, `config.status`, `aclocal.m4`, `win32/`, `otp_built`, `otp_doc_built`, and `make/autoconf/win*.config.cache`. `otp_built`/`otp_doc_built` are build marker files touched by the top-level Makefile.
- Files here are produced by two different configure scripts: `make/configure` (→ `output.mk`, `$(TARGET)/otp_ded.mk`) and `erts/configure` (→ `$(TARGET)/otp.mk`, `make_emakefile`, `ex_doc_wrapper`, `install_dir_data.sh`). This split is intentional but surprising.
- `otp_ded.mk` and `make/autoconf/*.static.config.cache` may be consumed *before* configure has run, so their version constants (`DED_MK_VSN`) must stay literal.
- There are no unit tests for these fragments; they are exercised by `./configure && make` and by the per-app test targets.

## Test Strategy
- No standalone test suite. Verify changes by running `./configure && make` (or the affected component's `make opt`/`make docs`) and, for doc changes, an app's `make docs`/`make man`.
- Silent-build behavior can be checked with `make V=1` (verbose) vs default (`V=0`).

## See Also
- `make/autoconf/CONTEXT.md` — OTP autoconf macro library and aux scripts (see routing table).
- Root `CONTEXT.md` — top-level `configure`/`otp_build`/`Makefile` entry points.
- `../erts/configure.ac` — generates `make/$(TARGET)/otp.mk` and several generated scripts here (sibling — read-only).
- `../system/` — system-level documentation (`docs.exs` doc build) that consumes `doc.mk`/`ex_doc.exs`.
- `../scripts/` — release/CI helper scripts (tar, license headers, dialyzer runner).

## Routing Table
- `make/autoconf/` → Shared autoconf aux files: `otp.m4` macro library, `config.guess`/`config.sub`/`install-sh`, `vendor.info`, `win*.config.cache.static`
- `../erts/configure.ac` → Generates `make/$(TARGET)/otp.mk`, `make/make_emakefile`, `make/ex_doc_wrapper`, `make/install_dir_data.sh` (sibling — read-only, escalate writes to root)
- `../lib/` → OTP applications that include these makefile fragments (sibling — read-only)
- `../system/` → System docs build that consumes the `doc.mk`/ex_doc pipeline (sibling — read-only)
