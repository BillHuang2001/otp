# erts/etc/ — launcher / user-facing layer of an Erlang installation

## Intent
The small programs, start-up scripts and installer parts that sit between the user and the runtime: the per-OS launchers (`erl`, `erlc`, `escript`, `dialyzer`, `typer`, `ct_run`, `erl_call` and the `heart`/`inet_gethost` port programs), the `run_erl`/`to_erl` pipe-logging daemons, the release `Install` script, the Windows service/installer programs, and the gdb/lldb Erlang-Term-Parser (ETP) debug toolbox.
This node is mostly **sources and templates** — the compilation rules and the release/install lists for everything here live in `common/Makefile.in`.
`erts/etc/Makefile` is only a dispatcher: `SUB_DIRECTORIES = common`, plus `win32` when `TARGET=win32`, else `unix`.
Not produced here: `epmd` (`../epmd/`), the emulator (`../emulator/`), the boot scripts (`../start_scripts/`).
Sibling areas: `../` (ERTS), `../emulator/`, `../lib_src/`, `../../lib/kernel/`, `../../lib/erl_interface/`.

## API Surface — what this node ships, and where

**Build outputs** go to `$(ERL_TOP)/bin/$(TARGET)/` (git-ignored `bin/`), objects to `$(ERL_TOP)/erts/obj$(TYPEMARKER)/$(TARGET)/`.
`erts/Makefile` target `local_setup` then copies/symlinks them into `$(ERL_TOP)/bin/` and generates the in-tree `bin/erl` and `bin/cerl` from the `unix/` templates.

| Area | Artifacts | Installed to |
|---|---|---|
| `common/` | `erlexec` (→ `erlexec.dll` on win32), `inet_gethost`, `heart`, `erlc`, `escript`, `ct_run`, `dialyzer`, `typer` | `erts-$(VSN)/bin` |
| `unix/` | `run_erl`, `to_erl`; templates `erl.src.src`→`erl.src`, `start.src`, `start_erl.src` (`INSTALL_EMBEDDED_DATA`/`PROGS`) | `erts-$(VSN)/bin` |
| `unix/` | `Install.src`→`Install` (`INSTALL_TOP`), `format_man_pages` (`INSTALL_MISC`), `setuid_socket_wrap.c` (`INSTALL_SRC`, "as an example") | release root; `misc/`; `erts-$(VSN)/src` |
| `win32/` | `erl.exe`, `erl_log.exe`, `erlsrv.exe`, `start_erl.exe`, `Install.exe`+`Install.ini`, `.res` resources, `.pdb` files | `erts-$(VSN)/bin`, release root, `usr/lib/icons` |
| `win32/nsis/` | official Windows setup `.exe` (built only by `otp_build installer_win32`) | — (not part of the release tree) |
| `unix/` | ETP toolbox (`etp-commands` generated from `etp-commands.in`, `etp*.py`, `etp_commands.erl`), `gcov-gen-html` | not installed |

**Install-time stage**: the user runs the shipped `<release>/Install <ERL_ROOT>`, which seds `%FINAL_ROOTDIR%` into `erl`/`start`, symlinks `bin/epmd`, copies the embedded programs, and writes `releases/start_erl.data` + `releases/RELEASES` and `start.boot`/`start.script`.

**Build-time generation inside this node** (all git-ignored via `erts/.gitignore`): `common/Install` ← `unix/Install.src`; `common/erl.src` ← `unix/erl.src.src`; `unix/etp-commands` ← `unix/etp-commands.in`; `win32/version.h` ← `win32/version.h.src`; `win32/Install.ini` ← `win32/Install.src`; and `common/$(TARGET)/Makefile` ← `common/Makefile.in` (`erts/configure.ac: AC_CONFIG_FILES([etc/common/$host/Makefile...)`).

## Constraints
- **Never edit generated files**; edit their sources: `Install.src`, `erl.src.src`, `etp-commands.in`, `version.h.src`, `Install.src` (win32), `Makefile.in`.
- **Placeholder conventions are not uniform**: `%NAME%` in the shell templates (`erl.src.src`, `cerl.src`, `start.src`, `start_erl.src`, `Install.src`) and in `version.h.src`; `@NAME@` in `etp-commands.in` and in `Makefile.in` (autoconf).
- **Two-stage substitution contract for `erl`/`start`/`start_erl`**: build time bakes `%VSN%`/`%EMULATOR%`/`%EMULATOR_NUMBER%` (and `%I_VSN%`/`%I_SYSTEM_VSN%` into `Install`); install time bakes `%FINAL_ROOTDIR%`. Changing either side breaks installs.
- `common/Makefile` (tracked, 27 lines) is only `include $(ERL_TOP)/make/run_make.mk`; the real makefile is the configured `common/$(TARGET)/Makefile`, invoked with `-f $(TARGET)/Makefile`, so its relative paths resolve against `common/`. Same pattern as `erts/emulator`.
- Only `TYPE=opt` and `TYPE=debug` are meaningful here; any other `TYPE` is `override`n to `opt` in `common/Makefile.in`.
- The whole directory is serialised behind `erts/lib_src` through the `$(ERTS_LIB)` = `../lib_src/obj/$(TARGET)/$(TYPE)/MADE` sentinel.
- The C sources include the `erts` `config.h` under `HAVE_CONFIG_H`; nothing here compiles without a configured tree.
- Everything under `unix/` must be POSIX `/bin/sh` (no bashisms); Android builds rewrite `/bin/sh`→`/system/bin/sh` in the generated `erl`, `cerl`, `start`, `start_erl`.
- Windows: `USING_VC=@MIXED_VC@` in `common/Makefile.in` splits `-MT`+`.res` (VC) from `-MD`+`rc.sh`-converted `.o` (mingw); the `mc`/`rc` steps are deliberately serialised by false dependencies because MSVC's message/resource compilers transiently create and delete a top-level `vc100.pdb`.

## Design Decisions
- **`erl` is a `/bin/sh` script, `erlexec` is a separate C program**: the shell script does runtime `ROOTDIR` discovery (walking parents for an `erts-<vsn>` directory, honouring `$ERL_ROOTDIR`, falling back to the baked-in root) so a copied/moved/symlinked release still works without recompiling, and platform tweaks are trivial `sed` edits; all command-line/`ERL_FLAGS`/`-args_file` processing, emulator type/flavor selection, `epmd -daemon` startup and the final `execv` stay in portable C shared with `cerl` and with Windows.
- **The other front-ends (`erlc`, `escript`, `dialyzer`, `typer`, `ct_run`) are small C programs, not scripts**: Windows has no `/bin/sh`, shells impose argv limits, and argv must be rewritten in place; each one builds an `erl …` command line and re-execs it. Only `erlc`'s compile-server mode (`ERLC_USE_SERVER`, `-server`) uses `lib/erl_interface` (`ei`) — the others link `erts_internal` only for `erl_printf`.
- **Platform split by subdirectory, not `#ifdef`** (matching `../emulator/{drivers,nifs,sys}/{common,unix,win32}`); `unix/` and `win32/` are mutually exclusive, picked by `erts/etc/Makefile`. The portable `erlexec.c` still carries `#ifdef __WIN32__` sections for the DLL/`erl.ini` path.
- **Windows uses `erl.exe` + `erl.ini` + `erlexec.dll`** instead of a shell script; installed trees ship *without* `erl.ini` (the exe deduces the erts dir) so they stay relocatable.
- **ETP is a generated gdb command file** (commands prefixed `etp-`/`etpf-`) rather than a gdb Python module, because the command body predates gdb's Python API; `etp.py` is the newer lldb implementation.
- **`win32/port_entry.c` overrides the CRT entry point** (`-entry:erl_port_entry`) rather than wrapping `main`, so `heart`/`inet_gethost` survive logoff/shutdown when hosted by a service.

## Known Issues
- `unix/README` and `unix/RELNOTES` are stale 1990s prose (SunOS/Interactive UNIX, `%VERSION%` substitution, "bundles", Erlang 4.3.1) — not current documentation.
- `unix/gdb-tools/` **does not exist in the repo**: it is cloned by `./otp_build download_gdb_tools` (upstream pinned by `make/gdb_tools_vsn`, https://github.com/erlang/otp-gdb-tools) and built from `common/`. `common/Makefile.in` `-include`s `../unix/gdb-tools/jit-reader.mk`, so its absence is expected and harmless.
- `%DYN_ERL_PATH%`, `%DYN_ROOTDIR_BASE_EXT%` and `%EMULATOR_NUMBER%` are leftovers of the removed `dyn_erl` mechanism: `erts/Makefile` still seds them, but the corresponding placeholders no longer exist in the templates — the substitutions are no-ops.
- `etp-commands.in` is ~158 KB / 5665 lines — grep it, never read it whole. Its `etp-` process-flag decoder mirrors flags in `../emulator/beam/erl_process.h` and must be updated together with them.
- `unix/format_man_pages` is vestigial (only runs when an installed release has `man/`, hardcodes old uname target detection); `unix/gcov-gen-html` is manual-only (needs perl + `lcov`) and is not installed.
- `unix/setuid_socket_wrap.c` is shipped only as an example, and has a live duplicate at `../../lib/inets/test/inets_socketwrap_SUITE_data/` — changes here are the upstream.
- `erts/Makefile` is tracked yet matched by `erts/.gitignore:/Makefile`, so ripgrep skips it by default — use `rg --no-ignore` when searching for `local_setup`/substitution logic.
- win32: `resource.h`'s menu/dialog/accelerator ids and the corresponding `beam.rc` blocks have no C consumer; `win32/Makefile`'s `INSTALL_PROGS` is a legacy subset (the full list comes from `common/Makefile.in`); `usr/lib` is created empty; `manifest.xml` must be revised for each new Windows release (asserted by `../../lib/kernel/test/os_SUITE.erl`).
- Windows test coverage is thin: `../../test/nt_SUITE.erl` schedules its real `erlsrv` service tests only on Windows ≤ 6.1, and `../../test/install_SUITE.erl` skips entirely on win32.

## Notes for Agents
- `common/Makefile.in` is the single place that wires the C sources of this whole node (including `unix/*.c` and `win32/*.c`) into the build — look there first when a file here appears unused.
- A bare `make` in `unix/` only produces `etp-commands`; a bare `make` in `win32/` only reaches `release_spec`. To actually compile, run `make` in `erts/etc` (or `erts/etc/common`) from a configured tree; on win32 the driver is `otp_build env_win32`/`env_mingw32`.
- Read `../../HOWTO/INSTALL-WIN32.md` for the three Windows toolchain environments before touching a `win32/*_tools/` shim; the three trees are near-duplicates and a functional change normally must be mirrored into all of them.
- Debug docs: `../doc/references/{run_erl,start,start_erl,erl,werl,erlc,escript,erlsrv}_cmd.md`, `../../system/doc/embedded/embedded.md`, `../../system/doc/system_principles/create_target.md`, `../../system/doc/tutorial/debugging.md`, `../../HOWTO/DEVELOPMENT.md`.

## Dependencies
- C compiler, GNU make, autoconf 2.72; `erts/include{,/internal}{,/$(TARGET)}` headers and the `erts_internal` library from `../lib_src`; `@ERTS_INTERNAL_X_LIBS@`, `-lm`, `@LIBRT@` (heart), `ws2_32`/`mswsock` (Windows).
- `../../lib/erl_interface` (`ei`) — only for `erlc`'s compile-server mode and the `erl_call` binary that is copied into `bin/`.
- unix runtime: `mkfifo`, pty (`openpty`/`forkpty`), `termios`/`ioctl`, `syslog`, `select`.
- ETP needs gdb or lldb (rr/emacs optional) and `erl`/`erlc` in `PATH`; `gcov-gen-html` needs perl + `lcov`; `format_man_pages` needs the platform's nroff family.
- Windows: MSVC (`cl`,`link`,`lib`,`rc`,`mc`,`mt`) or mingw-w64 (`gcc`,`windres`,`wine-wmc`) plus the `win32/*_tools/` shims; NSIS ≥ 3.0.6 for `win32/nsis/`.

## Test Strategy
- All `erts`-level suites need a built release (`erts/Makefile` sets `TEST_NEEDS_RELEASE=true`); run them with `make test` from `ERL_TOP` or `erts/`. Root `Makefile.in` `TEST_DIRS` includes `erts/test` (subject to `lib/SKIP-APPLICATIONS`).
- `../test/erlexec_SUITE.erl` — `erlexec` command-line/env/`-args_file` handling (`args_file`, `env`, `argument_separation`, `argument_too_large`, `zdbbl_dist_buf_busy_limit`, …).
- `../test/erlc_SUITE.erl` — `erlc` end-to-end; its `with_server`/`features_with_server` groups exercise the compile-server path.
- `../test/install_SUITE.erl` — release install layout (`install_SUITE_data/install_bin`); skips on win32.
- `../test/upgrade_SUITE.erl` — `start_erl`/`erl.src` substitution and `releases/start_erl.data`.
- `../test/run_erl_SUITE.erl` — `run_erl` + `to_erl` (unix only); the only automated coverage of `unix/`.
- `../test/nt_SUITE.erl` — Windows service (`erlsrv.exe`); `../test/otp_SUITE.erl` — general smoke test.
- Specs selecting them: `../test/system.spec`, `system_smoke.spec`, `system.dynspec`.
- **Coverage gaps**: no automated test for the ETP toolbox, `cerl`, `format_man_pages`, `gcov-gen-html`, the Windows installer/registry/Start-menu entries, or `Install.exe`.
- `cerl -asan`/`-valgrind` wire in the suppressions from `../emulator/asan/` and `../emulator/valgrind/` — those flavors are the intended way to exercise this C code under sanitizers.

## Routing Table
- `common/` → portable C launcher programs (`erlexec.c` = the `erl` launcher and emulator-argument processor, `inet_gethost.c`, `heart.c`, `erlc.c` incl. the `ei`-based compile server, `escript.c`, `ct_run.c`, `dialyzer.c`, `typer.c`, `etc_common.h`) **and** `Makefile.in` — the real build/release description for this entire node
- `unix/` → unix shell templates (`erl.src.src`, `cerl.src`, `start.src`, `start_erl.src`, `Install.src`), `run_erl.c`/`to_erl.c`/`safe_string.{c,h}` (logging-pty daemons), the ETP toolbox (`etp-commands.in` → `etp-commands`, `etp.py`, `etp-thr.py`, `etp-rr-run-until-beam.py`, `etp_commands.erl`/`.mk`), auxiliary scripts (`format_man_pages`, `gcov-gen-html`), `setuid_socket_wrap.c`, and the stale `README`/`RELNOTES`
- `win32/` → Windows launcher/resource sources (`erl.c`, `win_erlexec.c`, `init_file.c`, `Install.c`, `start_erl.c`, `erl_log.c`, `port_entry.c`, `version.h.src`), icons/`.rc`/`manifest.xml`, and the child areas `erlsrv/` (service wrapper), `nsis/` (installer build), `cygwin_tools/`, `msys_tools/`, `wsl_tools/` (near-duplicate MSVC/mingw shim trees)
- `../test/` → the suites covering this node (`erlexec_SUITE`, `erlc_SUITE`, `install_SUITE`, `upgrade_SUITE`, `run_erl_SUITE`, `nt_SUITE`, `otp_SUITE` + `system*.spec`) (sibling — read-only, escalate writes to `../`)
- `../emulator/beam/erl_etp.c` → emulator-side producer of the data the ETP commands decode (sibling — read-only)
- `../lib_src/` → `erts_internal`/`erl_printf`, which every program here links (sibling — read-only)
- `../../lib/kernel/` → Erlang-level counterparts (`heart.erl`, `inet_gethost_native.erl`, `erl_compile_server.erl`, `erl_epmd`) (outside this node — read-only)
