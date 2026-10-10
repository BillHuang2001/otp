# erts/etc/common — launcher programs (C) and generated front-end scripts

## Intent
The small C programs that an Erlang installation execs, plus the shell/INI templates generated next to them.
They sit between the user (`$ROOTDIR/bin/erl`, `erlc`, `escript`, `dialyzer`, `typer`, `ct_run`) and the runtime: they translate command lines, set up environment/variables and then `exec` either the emulator (`erlexec`) or `erl` itself (the thin front-ends).
Everything here is installed into `erts-$(VSN)/bin` (plus a few files into `$(RELEASE_PATH)`), never into an application `ebin`.
Platform counterparts: `../unix/` (shell templates plus the unix-only C sources `run_erl.c`, `to_erl.c`, `safe_string.c`, `setuid_socket_wrap.c`) and `../win32/` (all Windows-only C sources, `.rc` resources, `Install.src`/`Install.c`, `erlsrv/`).
Sources from `../unix/` and `../win32/` are **compiled by the `Makefile.in` in this directory** — `../unix/Makefile` only generates `etp-commands`.

## API Surface

### Programs produced (unix → win32)
| Source | Artifact | Runtime role |
|---|---|---|
| `erlexec.c` | `erlexec` → `erlexec.dll` | Program exec'ed by `$ROOTDIR/bin/erl` (a `/bin/sh` script generated from `../unix/erl.src.src`). Parses the entire `erl` command line and `ERL_FLAGS`/`ERL_AFLAGS`/`ERL_ZFLAGS`/`ERL_OTP<vsn>_FLAGS`/`-args_file`, takes `ROOTDIR`/`BINDIR`/`PROGNAME`/`EMU` from the environment (falling back to deriving them from `argv[0]`, or `erl.ini` on Windows), derives the emulator artifact name `$(EMU)[.<type>].<flavor>` (`-emu_type`, `-emu_flavor`, default flavor `smp`), optionally starts `epmd -daemon`, then `execv`s the emulator (`beam`, `beam.smp`, …). Also implements `-make`, `-man`, `-compile`, `-no_epmd`, `-proto_dist`, `-args_file`, `-configfd`, `-detached`, `-start_epmd`, plus the Windows-only `-start_erl`/`-boot`/`-config`/`-service_event` handling used by `start_erl.exe` and `erlsrv`. |
| `inet_gethost.c` | `inet_gethost` | `inet_gethost_native` port program: DNS/host lookup for `kernel`'s inet; a fork()-ed pool of workers on unix, a thread pool with per-request message queues on Windows; speaks a small binary protocol on stdin/stdout to the emulator. |
| `heart.c` | `heart` | `kernel`'s `heart` port program (`open_port({spawn,'heart'},[{packet,2}])`), started with `erl -heart`; reboots the system via `$HEART_COMMAND` when heartbeats (`$HEART_BEAT_TIMEOUT`, default 60 s) stop; links against `@LIBRT@` for `clock_gettime` (not on Android). |
| `erlc.c` | `erlc` | Compiler front-end: builds an `erl -s erl_compile compile_cmdline` command line and execs it. Additionally implements the **compile-server mode** (`-server`/`-no-server`, `$ERLC_USE_SERVER`), which is the only place `erl_interface` (`ei_*`, `-lei`) is used; it starts the server with `-sname erlc_compile_server_<host> -setcookie erlc_compile_server_cookie -hidden -detached -kernel start_compile_server true` and RPCs `erl_compile_server:compile/1` (in `lib/kernel/src/erl_compile_server.erl`) over distribution, falling back to local compilation on any failure. |
| `escript.c` | `escript` | escript front-end: resolves the `.escript` script (or `<progname>.escript` when invoked under another name), reads `%%!` shebang args, sets `$ESCRIPT_NAME`, and execs `erl +B -noshell -boot $ROOT/no_dot_erlang -run escript start -extra …`. Emulator override: `$ESCRIPT_EMULATOR`. |
| `dialyzer.c` | `dialyzer` | Dialyzer front-end: execs `erl +B -boot $ROOT/no_dot_erlang -run dialyzer plain_cl …`. Emulator override: `$DIALYZER_EMULATOR`. |
| `typer.c` | `typer` | Typer front-end: execs `erl +B -boot $ROOT/no_dot_erlang -run typer start …`, `--shell` suppresses `-noinput`. |
| `ct_run.c` | `ct_run` | Common Test front-end: picks `-sname`/`-name` (default `ct`), then `-s ct_run script_start` / `ct_webtool script_start vts` / `ct_master` modes based on `-vts`/`-shell`/`-ctmaster`/`-ctname`/`-browser`/`-erl_args`. |
| `etc_common.h` | — | Shared include preamble for all of the above: `config.h`, platform libs, `MAXPATHLEN` fallback, `erl_printf.h`, `DEBUG`-only `ASSERT`. |
| `../unix/run_erl.c`, `../unix/to_erl.c`, `../unix/safe_string.c` | `run_erl`, `to_erl` | Unix-only rules in this `Makefile.in` (no `ei`, only `$(LIBS)`); `safe_string.o` is shared by both. |
| `../win32/{erl,erl_log,Install,start_erl,win_erlexec,init_file,port_entry}.c`, `../win32/erlsrv/*.c` | `erl.exe`, `erl_log.exe`, `Install.exe`, `start_erl.exe`, `erlsrv.exe` | Windows-only; `erl.exe` `LoadLibrary`s `erlexec.dll` and calls its `win_erlexec` entry point, which in turn `LoadLibrary`s `beam.dll` (`start_emulator()` in `win_erlexec.c`). |
| `lib/erl_interface/bin/$(TARGET)/erl_call` | `erl_call` | **Not built here** — simply `cp`-ed (plus `.pdb` on Windows); requires `lib/erl_interface` to be built first. |

### Generated files in this directory (git-ignored)
- `Install` — from `../unix/Install.src`, substituting `%I_VSN%`, `%EMULATOR%`, `%EMULATOR_NUMBER%`, `%I_SYSTEM_VSN%`. Post-install script run once in a release: replaces `%FINAL_ROOTDIR%` in `erl.src` → `erl`, creates `start`/`start_erl` from `start.src`/`start_erl.src`, writes `releases/start_erl.data`, copies/links `epmd`, `run_erl`, `to_erl`.
- `erl.src` — from `../unix/erl.src.src`, substituting `%EMULATOR%`, `%EMULATOR_NUMBER%`, `%VSN%`. **`%FINAL_ROOTDIR%` is intentionally left for `Install`/`local_setup` to fill in.**
- `Install.ini` (win32) — from `../win32/Install.src`, substituting `%I_VSN%`, `%I_SYSTEM_VSN%`.
- `../win32/version.h` — from `../win32/version.h.src`, substituting `%ERTS_VERSION%`, `%ERTS_VERSION_INTS%`, `%OTP_VERSION%`, `%OTP_VERSION_INTS%` (max 4 comma-separated integers), `%YEAR%`.
- `<TARGET>/Makefile` — the **real** Makefile for this directory.

### Makefile variables (release locations)
`release_spec` installs into `$(RELEASE_PATH)`:

| Variable | unix | win32 |
|---|---|---|
| `INSTALL_PROGS` | `inet_gethost`, `heart`, `erlexec` + `INSTALL_EMBEDDED_PROGS` → `erts-$(VSN)/bin` | `inet_gethost.exe`, `heart.exe`, `erlsrv.exe`, `erl.exe`, `erl_log.exe`, `erlexec.dll` + embedded progs → `erts-$(VSN)/bin` (plus matching `.pdb` files) |
| `INSTALL_EMBEDDED_PROGS` | `typer`, `dialyzer`, `erlc`, `escript`, `ct_run`, `run_erl`, `to_erl`, `erl_call` | `typer.exe`, `dialyzer.exe`, `erlc.exe`, `start_erl.exe`, `escript.exe`, `ct_run.exe`, `erl_call.exe` |
| `INSTALL_EMBEDDED_DATA` | `../unix/start.src`, `../unix/start_erl.src` | (empty) |
| `INSTALL_TOP` | `Install` → `$(RELEASE_PATH)/` | `Install.ini` → `$(RELEASE_PATH)/` |
| `INSTALL_TOP_BIN` | (empty) | `$(BINDIR)/Install.exe` → `$(RELEASE_PATH)/` |
| `INSTALL_MISC` | `../unix/format_man_pages` → `misc/` | (unset) |
| `INSTALL_SRC` | `../unix/setuid_socket_wrap.c` → `erts-$(VSN)/src` (example) | `../win32/start_erl.c`, `../win32/Nmakefile.start_erl` → `erts-$(VSN)/src` |
| `TEXTFILES` | `Install erl.src` (build-dir artifacts) | `Install.ini` |
| `erl.src` install | yes (skipped for win32) | — |

`INSTALL_OBJS`, `INSTALL_LIBS`, `INSTALL_INCLUDES` are empty for both platforms.
Android (`*-android*`) additionally rewrites `/bin/sh` → `/system/bin/sh` in the installed `erl.src`, `start.src` and `start_erl.src`.
`local_setup` (in `../Makefile`) does **not** run `release_spec`: it copies `$(ERL_TOP)/bin/$(TARGET)/*` into `$(ERL_TOP)/bin/` and generates `$(ERL_TOP)/bin/erl`/`cerl` from `../unix/erl.src.src`/`cerl.src` with `%FINAL_ROOTDIR%` = `$(ERL_TOP)`.

## Constraints
- The tracked `Makefile` (26 lines) is only a wrapper around `make/run_make.mk`; the **real** Makefile is `Makefile.in`, configured by `erts/configure` into the git-ignored `./$(TARGET)/Makefile` (`AC_CONFIG_FILES([etc/common/$host/Makefile:etc/common/Makefile.in])`). Edit `Makefile.in` only, then run `./otp_build update_configure`.
- Relative paths in `Makefile.in` (`../unix`, `../win32`, `../../vsn.mk`, `$(TARGET)/Makefile` prerequisites) resolve against the **cwd** `erts/etc/common`, not against `$(TARGET)/Makefile`'s location, because make is invoked with `-f $(TARGET)/Makefile` from here.
- Only `TYPE=opt` and `TYPE=debug` work here: anything else is silently forced back to `opt` (`override TYPE=opt`). `debug` adds `-DDEBUG @DEBUG_FLAGS@` and uses the `.debug` object/lib typemarkers.
- Never edit the generated files listed above; edit their `.src`/`.src.src` templates in `../unix/` / `../win32/`.
- On Windows `erlexec.c` is compiled with `-DERL_RUN_SHARED_LIB=1 -DBUILD_TYPE=\"-$(TYPE)\"`, turning `main()` into the exported DLL entry point `win_erlexec`; the same file is a normal `main()` program everywhere else.

## Design Decisions
- **`erlexec` is separate from `beam`** so the emulator binary stays free of installation-layout knowledge: `erlexec` owns the shell-level contract (`R_OTP_ROOTDIR`, `ROOTDIR`/`BINDIR`/`PROGNAME`/`EMU` env, `ERL_*_FLAGS`, `-args_file`, `-make`, `-man`, `epmd` startup, type/flavor selection) and passes a fully normalised `-- … -root … -bindir … -progname …` argv to the emulator. It also preserves `exec` semantics so `erl` owns the tty/pid and can be killed directly.
- **Type/flavor suffixes live in `erlexec`**, not in the emulator's name: the emulator is always `beam` + `[.type].flavor` (default `smp`), so several build types can coexist in one `bin/$(TARGET)` and the selector is a command-line flag.
- **The front-ends are C, not shell scripts**, because they must work on Windows (no usable `/bin/sh`) and need precise `argv` manipulation: they pre-allocate an oversized argv buffer, push the emulator name at the base, insert the Erlang command in the middle and shift the base up at the end, then `execvp` `erl` (one process, no shell quoting/length limits).
- **Front-ends stay library-light**: they build an `erl` command line and re-`exec` `erl` rather than embedding the compiler/analysis logic. They only link `erts_internal` for `erts_snprintf`/`erts_vsnprintf` (implemented in `../lib_src/common/erl_printf.c`) and `erl_misc_utils`. Only `erlc`'s optional compile-server mode adds `erl_interface`/`ei` and real distribution handling.
- **Same source for unix and win32** (`#ifdef __WIN32__` inside `erlexec.c`, `escript.c`, …): the platform split for *files* is by subdirectory (`../unix`, `../win32`), not by prefixing each file, and `erlexec.c`'s Windows-only runtime logic is factored out into `../win32/win_erlexec.c`/`init_file.c`.
- The `Install` shell script (unix) and `Install.exe` (win32) exist because a release tarball cannot be relocated at build time; the `%FINAL_ROOTDIR%`/`%EMULATOR%` placeholders are resolved at install time.

## Known Issues / Gotchas
- `$(EMULATOR_NUMBER)` is not defined anywhere in the tree, so `%EMULATOR_NUMBER%` expands to the empty string in the generated `erl.src`/`Install` (a leftover from the removed `dyn_erl`, see `git log -S dyn_erl`). Do not "fix" it by guessing a value.
- Windows parallel-make hack: `$(MC_OUTPUTS)`/`$(RC_GENERATED)` carry deliberate false dependencies (sentinel `$(OBJDIR)/LOGMESS-GENERATED`) so the message/resource compilers finish before other compilations; the comment explains the `vc100.pdb` race. Do not "clean up" these dependencies.
- `Install`/`erl.src`/`Install.ini` depend on `$(TARGET)/Makefile`, i.e. they exist only in a configured tree; referencing them from an unconfigured checkout fails.
- Every program that uses `erts_internal` depends on the sentinel target `$(ERTS_LIB)` = `erts/lib_src/obj/$(TARGET)/$(TYPE)/MADE`, whose recipe runs `make $(TYPE)` in `erts/lib_src`; this is what serialises the `erts/etc` build behind the runtime-lib build.
- `-include ../unix/gdb-tools/jit-reader.mk` (line 221) refers to a directory that is **not in the repository**; it is cloned into `erts/etc/unix/gdb-tools` by `./otp_build download_gdb_tools` (repo/commit pinned by `make/gdb_tools_link`, `make/gdb_tools_vsn`).
- `etc_common.h` neutralises `system()` to a no-op under `__IOS__`, and maps `snprintf` → `_snprintf` on Windows; code here must not rely on `system()`.
- `escript.c`, `ct_run.c`, `typer.c`, `dialyzer.c` on Windows take `wmain()` with `wchar_t` argv and convert to UTF-8 first; on unix `main()` is used. `erlc` does the same conversion inline.
- `clean` only removes objects for a list of hard-coded `*.o` names in `$(ERL_TOP)/erts/obj*/$(TARGET)`; new object files must be added there.
- Windows configuration is `erl.ini` (generated by `../win32/cygwin_tools/make_local_ini.sh` and read by `init_file.c`) plus `Install.ini`; there is no `Start.ini` in the tree.

## Notes for Agents
- To build/check these programs you need a configured tree: `./configure && make` from `ERL_TOP`, or `cd erts/etc/common && make TYPE=debug` (which goes through the `Makefile` wrapper).
- Object output: `$(ERL_TOP)/erts/obj$(TYPEMARKER)/$(TARGET)` (`erts/obj/…` or `erts/obj.debug/…`); executable output: `$(ERL_TOP)/bin/$(TARGET)/` — both are git-ignored build trees, do not look for code there.
- `erlexec.c` is a ~2.5 kloc monolith by design; navigate it by the static function list near the top (`get_parameters`, `initial_argv_massage`, `add_args`/`add_Eargs`/`add_arg`, `start_epmd_daemon`, `read_args_file`). Two argv buckets are kept: `Eargsp` = everything before the `--` separator (goes to the emulator) and `argsp` = everything after it.
- `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `otp_ded.mk` and `output.mk` are generated from `make/*.in` by `erts/configure`; they are absent in an unconfigured checkout, so the `include`s at the top of `Makefile.in` cannot be resolved without configuring.
- `$(TARGET)/` subdirectories under `erts/etc/` are configure output and are ignored by the root `.gitignore` (host-triple pattern).
- Every `.c` here is live: `Makefile.in` gives each its own `$(OBJDIR)/<stem>.o: <stem>.c` rule (build class A) and lists its binary in `INSTALL_PROGS`/`INSTALL_EMBEDDED_PROGS`; `ct_run.c`, `dialyzer.c`, `erlc.c`, `erlexec.c`, `escript.c`, `heart.c`, `typer.c` all `#include "etc_common.h"` (include class B) — `inet_gethost.c` does not, but is still compiled directly. The mechanical dead-source checker `../../emulator/beam/check_dead_sources.py` is scoped to `erts/emulator/**` only and does **not** scan this node.

## Dependencies
- C compiler, GNU make, plus the `$(ERL_TOP)/erts/include{,/internal}{,/$(TARGET)}` headers and the `erts_internal` static/shared library (`../lib/internal/$(TARGET)/liberts_internal[_r][.debug]`, built from `erl_printf_format.c` + `erl_printf.c` + `erl_misc_utils.c`).
- `lib/erl_interface` (`-lei`, `-I lib/erl_interface/include` for `ei.h`) — needed for `erlc`'s compile-server mode and for the `erl_call` binary that is copied in.
- `@ERTS_INTERNAL_X_LIBS@` (e.g. `-lkstat` on Solaris) `-lm` (unix), `@LIBRT@` for `heart`; `ws2_32`/`mswsock` on Windows.
- Windows-only: MSVC/mingw toolchain, `@MIXED_VC@` (`USING_VC`) selecting `-lerts_internal_r` and the `mc` output flavour (`.rc`/`.res` for VC, `.rc`/`.o` for mingw), resource/message compilers `rc.sh`/`mc.sh` from `../win32/{cygwin_tools,msys_tools,wsl_tools}/vc/`.

## Test Strategy
- All `erts`-level tests need a built release (`erts/Makefile` sets `TEST_NEEDS_RELEASE=true`); run them via `make test` from `ERL_TOP` or `erts/`.
- `../test/erlexec_SUITE.erl` — `erlexec`'s command-line parsing/env handling: `args_file`, `evil_args_file`, `missing_args_file`, `env`, `args_file_env`, `unicode_args_file`, `otp_7461(_remote)`, `argument_separation`, `argument_with_option`, `zdbbl_dist_buf_busy_limit`, `long_path_env`, `argument_too_large`.
- `../test/erlc_SUITE.erl` — `erlc` end-to-end; its `with_server`/`features_with_server` groups set `ERLC_USE_SERVER=yes`, exercising the compile-server path.
- `../test/install_SUITE.erl` (`install_SUITE_data/install_bin`) — install/layout of the release, i.e. the `release_spec`/`INSTALL_*` destinations above; `../test/upgrade_SUITE.erl` exercises `start_erl`/`erl.src` substitution and `releases/start_erl.data`; `../test/run_erl_SUITE.erl` covers `run_erl`/`to_erl`; `../test/otp_SUITE.erl` is the general smoke suite.
- `../test/Makefile` lists the suites; `system.spec` / `system_smoke.spec` / `system.dynspec` select them for a full system run.

## Routing Table
- `Makefile.in` — the real build/release description (compiles this directory *and* `../unix/*.c` / `../win32/*.c`); `Makefile` is the wrapper.
- `erlexec.c` — `erl` command-line/env handling, emulator/type/flavor selection, `epmd` startup, exec.
- `inet_gethost.c` — `inet_gethost_native` port program (unix fork pool / Windows thread pool).
- `heart.c` — `heart` port program (`kernel`'s `heart`).
- `erlc.c` — `erlc` front-end incl. `erl_interface`-based compile server.
- `escript.c`, `ct_run.c`, `dialyzer.c`, `typer.c` — the `-run <tool> start` front-ends.
- `etc_common.h` — shared include preamble for all of the above.
- `../unix/` → shell templates (`erl.src.src`, `Install.src`, `start.src`, `start_erl.src`, `cerl.src`) and unix-only C (`run_erl.c`, `to_erl.c`, `safe_string.c`, `setuid_socket_wrap.c`) — read-only from here, escalate writes to `../` (sibling).
- `../win32/` → Windows-only C/`.rc`/INI sources (`erl.c`, `win_erlexec.c`, `init_file.c`, `Install.c`, `start_erl.c`, `erl_log.c`, `port_entry.c`, `version.h.src`, `erlsrv/`, `nsis/`) — read-only from here (sibling).
- `../gdb-tools/` → **does not exist in the repo**; downloaded by `otp_build download_gdb_tools`.

## See Also
- `../Makefile` (`erts/Makefile`) → `local_setup` (copies these artifacts into `$(ERL_TOP)/bin`), `$(TYPES)` and `release` targets that drive this directory.
- `../CONTEXT.md` → `erts` application layout, `VSN`, preloaded modules, `bin/` build tree.
- `../lib_src/CONTEXT.md` → `erts_internal`, `erl_printf`, `erl_misc_utils`, `yielding_c_fun`.
- `../emulator/beam/bif.tab`, `../preloaded/` → where `erlexec`'s output ends up being interpreted (the emulator side of `-root`/`-bindir`/`-progname`).
- `../../lib/kernel/` → Erlang-side counterparts: `heart.erl`, `inet_gethost_native.erl`, `erl_epmd`, `code`.
- `../../lib/erl_interface/` → `ei` library and the `erl_call` binary copied into `bin/`.
- `../../HOWTO/` → INSTALL/TESTING/DEVELOPMENT guides for building and testing a release.
