# erts/start_scripts/ — OTP boot scripts

## Status
Complete. Leaf directory: no subdirectories, no child CONTEXT.md files; all knowledge is in this file.

## Intent
Generate the Erlang **boot scripts** shipped with OTP.
A boot script is the precomputed list of modules to load and applications to start, in start order; `erl` reads it at startup via `-boot`.
This directory turns small `.rel.src` release-spec templates into version-substituted `.rel` files, compiles them into `.script` (human-readable, `systools:make_script`/`erl_compile` → `systools:compile_rel`) and `.boot` (`systools:script2boot`) form, and produces the `RELEASES` file.

## API Surface
Tracked inputs (all others in this dir are generated and git-ignored):
- `Makefile` — builds everything; `SS_ROOT = $(ERL_TOP)/erts/start_scripts`, scratch dir `$(SS_ROOT)/tmp`.
- `start_clean.rel.src` — release libs `kernel`, `stdlib` → `start_clean.rel/.script/.boot`.
- `start_sasl.rel.src` — `kernel`, `stdlib`, `sasl` → `start_sasl.rel/.script/.boot`.
- `no_dot_erlang.rel.src` — same apps as `start_clean` but compiled with `+no_dot_erlang` (skips loading `~/.erlang`) → `no_dot_erlang.rel/.script/.boot`.
- `start_all_example.rel.src` — **example/template only**: `kernel`, `stdlib`, `sasl`, `os_mon`, `inets`, `snmp`, `mnesia`. No `.script`/`.boot` is ever built or installed from it; only `start_all_example.rel` is produced and installed.

Boot modes (see `system/doc/system_principles/system_principles.md`):
- `start.script`/`start.boot` — same content as `start_clean`; the *default* boot file chosen at install time (also installed as `releases/$(SYSTEM_VSN)/start.boot`).
- `start_clean.boot` — kernel + stdlib, no SASL (minimal startup).
- `start_sasl.boot` — kernel + stdlib + sasl (default when the installer answer is "no" to minimal startup).
- `no_dot_erlang.boot` — like `start_clean` but does not read the user's `.erlang`.
- `start_all_example.rel` — documentation example of a richer release spec.

Make targets:
- `script` (also `debug`/`opt`/`lcnt`) — default target: builds the `RELEASES.src` plus `$(SS_ROOT)/{start_clean,start_sasl,no_dot_erlang}.{script,boot}` (`INSTALL_SCRIPTS`). Invoked from the root `Makefile.in` `start_scripts` target as `cd erts/start_scripts && ERL_TOP=... $(MAKE) script`.
- `rel` — builds only the `.rel` files (`REL_SCRIPTS`: `start_clean.rel`, `start_sasl.rel`, `start_all_example.rel`, `no_dot_erlang.rel`).
- `$(ERL_TOP)/bin/start.script`, `.../bin/start_sasl.script`, `.../bin/start_clean.script`, `.../bin/no_dot_erlang.script` — special targets invoked from `erts/Makefile` `local_setup` (see `includes` diff below); compiled with `+otp_build` so paths become `$ROOT/lib/<app>/ebin` (dev/source-tree form, no version dir).
- `bootstrap_scripts` — invoked by the root `Makefile.in` primary-bootstrap sequence; writes `$(TESTROOT)/bin/{start.script,start_clean.script,no_dot_erlang.script}` from the bootstrap's own `kernel`/`stdlib` ebin dirs.
- `release_spec` — installs `RELEASES.src` into `$(RELEASE_PATH)/releases/` and the scripts into `$(RELEASE_PATH)/releases/$(SYSTEM_VSN)/`, and installs `start_clean.script`/`start_clean.boot` a second time as `start.script`/`start.boot`.
- `clean`, `docs`, `release_docs_spec`, `TRUE` (no-ops).

Outputs consumed elsewhere:
- `erts/etc/unix/Install.src` (and the win32 `Install`) substitute the `%ERL_ROOT%` placeholder in `RELEASES.src` into `$ERL_ROOT/releases/RELEASES` at install time.
- The generated boot scripts are what `erts/etc/unix/start.src`/`start_erl.src` (and `run_erl`) pass to `start_erl` for a release boot.

## Constraints
- **Generated, never edited/hand-written**: `*.rel`, `*.script`, `*.boot`, `RELEASES.src`, `tmp/` — all git-ignored by `erts/.gitignore`. Edit the `.rel.src` templates instead.
- `Makefile` begins with `.NOTPARALLEL:` — the targets here have interleaved scratch-dir (`tmp/`) and output-name dependencies and must not run in parallel.
- **Version substitution is `sed`-based**: `%SYS_VSN%` → `$(SYSTEM_VSN)` (from `make/otp.mk.in`/configure), `%ERTS_VSN%` → `$(VSN)` (`erts/vsn.mk`), `%KERNEL_VSN%`/`%STDLIB_VSN%`/`%SASL_VSN%`/`%OS_MON_VSN%`/`%MNESIA_VSN%`/`%SNMP_VSN%`/`%INETS_VSN%` from the included `lib/*/vsn.mk` (`VSN` for snmp comes from `$(SNMPEA_VSN)` — see the `start_all_example` rule). Adding an app to a `.rel.src` requires adding its `vsn.mk` include and a matching `sed` rule.
- Requires a **working `erlc`/`erl`** (and the built `lib/kernel`,`lib/stdlib`/`lib/sasl` ebins for `-I` paths) to run `systools`: this directory cannot build before the compiler/kernel/stdlib exist, so in a source build it runs after bootstrap (the `bootstrap_scripts` target is the one exception, using `$(BOOTSTRAP_TOP)/lib/*/ebin` via `BOOTSTRAP_SCRIPT_PATH`).
- `RELEASES.src` is **not** generated for `TARGET=win32` (`RELEASES_SRC` is emptied); on Windows the versions are installed only into `releases/$(SYSTEM_VSN)/`.
- The `%ERL_ROOT%` marker must stay in the generated `RELEASES` (via `release_handler:create_RELEASES("%ERL_ROOT%", ...)`).

## Design Decisions
- **Templates + `sed` instead of a generator program**: app versions live in exactly one place (`lib/*/vsn.mk`) and are substituted textually into a plain `.rel` — the same input format as any user release, so the OTP-internal path and the `systools` path are identical.
- **Boot scripts are pre-generated per release, not computed at runtime**: `start.boot` is a flat, deterministic list of load/start instructions loaded by `init` immediately after the preloaded modules are brought up; computing it at boot would require a compiler plus the version dirs to already be resolvable.
- **Three app-set variants are kept as separate templates** (`start_clean`, `start_sasl`, `start_all_example`) rather than generated by a parameterized rule, because their app lists differ structurally (and `start_all_example` is documentation).
- **`start.script` is byte-for-byte `start_clean.script`** — the installer simply copies it; there is no separate `start.rel.src`.
- **`no_dot_erlang` is the same release spec as `start_clean`** — the difference is entirely in the compile flags (`+no_dot_erlang`), not in the app set.
- **`+otp_build` paths are `$ROOT`-relative and unversioned** (`$ROOT/lib/<app>/ebin`): a boot script generated with `+otp_build` works both in the build tree (`bin/`) and after installation, because `$ROOT` is resolved by `init` at boot from `releases/RELEASES`/`start_erl.data`. `systools_make:cr_path/5` gives `otp_build` a dedicated clause distinct from the normal (`absname`) and `local` path forms; the flag is undocumented/secret and OTP-internal only.
- **The `erlc` `.rel`/`.script` magic is in stdlib, not here**: `lib/stdlib/src/erl_compile.erl` maps `".rel" -> {systools, compile_rel}` and `".script" -> {systools, script2boot}`; `+otp_build`, `+no_warn_sasl`, `+no_module_tests`, `+no_dot_erlang` are handled (and ignored/passed through) in `lib/sasl/src/systools_make.erl`.

## Known Issues
- `start_all_example.rel` is installed (via `REL_SCRIPTS` in `release_spec`) but no script/boot is built from it — do not expect `start_all_example.boot` to exist.
- Adding a new boot mode means touching several places at once: a new `.rel.src`, a new `sed` rule, an entry in `REL_SCRIPTS`/`INSTALL_SCRIPTS`, a `$(ERL_TOP)/bin/*.script` target plus the `erts/Makefile` `local_setup` invocation, and (if it should be the default) the installer logic.

## Notes for Agents
- The `tmp/` scratch dir (`SS_TMP`) is created by `$(INSTALL_DIR)` in each rule; `erlc` is run with `cd $(SS_TMP)` because `systools` writes intermediates relative to cwd. Do not "clean up" the `cd`.
- `$(SS_ROOT)/*.script|.boot` (this dir) are the *build outputs* consumed by `release_spec`; `$(ERL_TOP)/bin/*.script` are a separate `+otp_build` compilation for the dev tree. They are not interchangeable.
- The `.rel.src` files are plain Erlang terms with `sed` placeholders inside quoted strings — the `%...%` markers must survive any reformatting.

## Dependencies
- A working `erl`/`erlc` (Erlang/OTP itself), the `sasl` application (`systools`, `release_handler`), and the `kernel`/`stdlib`/`sasl` (+ `os_mon`,`mnesia`,`snmp`,`inets` for the example) ebin dirs.
- Build-system includes: `../vsn.mk` (erts), `lib/{kernel,stdlib,sasl,os_mon,mnesia,snmp,inets}/vsn.mk`, `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `$(ERL_TOP)/make/otp_release_targets.mk`.

## Test Strategy
- No test suite lives in this directory. Boot scripts are exercised indirectly:
  - `lib/sasl/test/systools_SUITE.erl` (`no_dot_erlang_script`, `no_sasl_script`) checks the generation logic.
  - `lib/sasl/test/release_handler_SUITE.erl` (`no_dot_erlang`) and `lib/kernel/test/init_SUITE.erl` start the VM with these boots.
  - `erts/test/upgrade_SUITE.erl` installs a release and copies a `.boot` to `bin/start.boot`.
  - `lib/reltool/` (`reltool_utils.erl` defines a `no_dot_erlang` rel) and `lib/sasl/doc`, `system/doc/system_principles/`.
- To verify locally after a full build: `ls "$ERL_TOP"/bin/*.script` and `bin/erl -boot no_dot_erlang` (must not read `~/.erlang`).

## Routing Table
This is a leaf node (no subdirectories) — the entire directory is documented above.
Cross-references (siblings, read-only from here — escalate writes to `../`):
- `../etc/unix/start.src`, `../etc/unix/start_erl.src` → the release-boot launcher path that consumes the installed boot scripts
- `../etc/unix/Install.src`, `../etc/win32/` → install-time `%ERL_ROOT%` substitution of `RELEASES.src`
- `../Makefile` → `local_setup` / `ERTSDIRS` (`NO_START_SCRIPTS` gates inclusion)
- `../../Makefile.in` → `start_scripts`, `local_setup`, and the `bootstrap_scripts` bootstrap sequence
- `../../lib/stdlib/src/erl_compile.erl`, `../../lib/sasl/src/systools_make.erl`, `../../lib/sasl/src/release_handler.erl` → boot-script generation (`compile_rel`, `script2boot`, `create_RELEASES`)
