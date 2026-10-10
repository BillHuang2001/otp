# erts/etc/win32/cygwin_tools/ — Cygwin toolchain shims for the Windows build

## Intent
Bash wrappers that let a **Cygwin** environment drive the *native* Microsoft toolchain (Visual C++), and optionally mingw/gcc, when building Erlang/OTP with `TARGET=win32`.
They exist so `configure` and `make` see a Unix-ish compiler/linker/archiver/`erlc`/`erl`/`javac` interface while the actual work is done by `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `erl.exe`, `erlc.exe`, `javac.exe`.
They convert Cygwin (POSIX) paths to paths MS tools understand (`cygpath -m/-w/-d`) and translate GCC-style flags into MSVC flags.
Nothing here is compiled as part of the normal build; the only compiled artifact is `coffix.exe`, built on demand by `emu_cc.sh`.
`otp_build env_win32` puts this directory (plus `vc/` or `mingw/`) first on `PATH`, and `make/autoconf/win*.config.cache.static` pins `CC=cc.sh`, `AR=ar.sh`, `RANLIB=true`, `JAVAC=javac.sh` per target.

## API Surface
Top level (all `#!/bin/sh` bash scripts, all shadow the real tools via `PATH`):
- `erl` — wrapper for `erl.exe`: converts `-I/path`, `-o/path` and absolute-path arguments with `cygpath -m`, exports `ERL_TOP` in mixed (`c:/...`) form, then `eval erl.exe <converted args>`. Makefiles use `ERL = erl -boot start_clean` (see `make/otp.mk.in`), so this wrapper is what `make` actually runs.
- `erlc` — wrapper for `erlc.exe`: same conversion plus `-pa<path>`; additionally supports `--echo_only` (print the converted command line instead of running it) and rewrites `+'{preproc_flags,"..."}'` by recursively invoking itself with `--echo_only`. The `preproc_flags`/`--echo_only` support is legacy and referenced nowhere else in the tree today.
- `javac.sh` — wrapper for `javac.exe`: converts `CLASSPATH` (`cygpath -m -p`), `-I/-o/-d` paths and absolute args. Found by `configure` via `AC_CHECK_PROGS(JAVAC, javac.sh javac ...)` (`erts/configure.ac`), pinned in `make/autoconf/win*.config.cache.static`.
- `make_local_ini.sh <rootdir>` — writes `<rootdir>/bin/erl.ini` with `[erlang] Bindir=<rootdir>/bin/win32`, `Progname=erl`, `Rootdir=<rootdir>`. Called from the win32 branch of `erts/Makefile` `local_setup` (uses `$ERL_TOP` when no argument is given). `Bindir` is the directory `bin/erl.exe` loads `erlexec.dll` from.
- `make_bootstrap_ini.sh <rootdir> <bindir>` — same INI but with an explicit `Bindir`. Called from the win32 branch of the root `Makefile.in` `bootstrap_setup` (`$BOOTSTRAP_ROOT/bootstrap` + `$ERL_TOP/bin/$(TARGET)`) to point the copied bootstrap `erl.exe`/`erlc.exe` at the freshly built `erlexec.dll`.
- `w32_path.sh -m|-w|-d|-u [-a] <path>` — thin `cygpath` wrapper: `-m` mixed (`c:/...`), `-w` windows (single backslashes), `-d` *doubled* backslashes (for INI/manifest use), `-u` unix, `-a` absolute. Must stay on `PATH` for the whole build: used by `make/dep.mk`, `make/test_target_script.sh`, `erts/emulator/utils/make_preload`, `erts/etc/win32/nsis/Makefile` and `lib/inets/src/*/Makefile`.
- `reg_query.sh <key> <valuename>` — queries the 64-bit registry: writes a temp `cmd` batch file (in `/tmp`), forces `%WINDIR%\sysnative\reg.exe` when running inside a 32-bit virtualized environment, and strips the `REG_*` prefix from the output. Currently **not referenced** by the build (legacy helper).
- `vc/` → see `vc/CONTEXT.md`; `mingw/` → see `mingw/CONTEXT.md`.

### How the INI files must be written
`erts/etc/win32/init_file.c` escapes `\` and `;` with a leading backslash when writing, and `erl.c` uses that parser, so paths written by these scripts go through `sed 's,\\\,\\\\\\\\,g'` (single backslash → doubled). Change the escaping in both places together.

## Constraints
- These are bash/sh scripts executed by Cygwin (`erl.exe`-style Windows binaries are invoked, so the scripts run under `sh` while their children are Windows programs). Do **not** assume a POSIX-only environment, and do not rewrite them as `/bin/sh`-only portable code without checking Cygwin's `cygpath` behaviour.
- Paths must be quoted and converted before being handed to `cl.exe`/`link.exe`/`lib.exe`/`rc.exe`/`mc.exe`; unconverted `/c/foo` paths silently fail.
- `%`-free, quoted `eval` command construction is the established style (`CMD="..."; eval cl.exe $CMD`) — keep it, it is what makes argument re-splitting work.
- Do not edit `../msys_tools/` or `../wsl_tools/` from here; changes must be reported to the parent of this node so it can decide on mirroring (see Notes for Agents).

## Design Decisions
- **Shims instead of native configure support**: the OTP build system is `configure`/`make`-based; rather than teaching `configure` about MSVC, the wrappers implement a GCC-ish command line on top of MSVC (`cc.sh` accepts `-c`, `-o`, `-I`, `-O*`, `-g`, `-MM`, `-l*`, `-L*`, `-MT/-MD/-ML`). `LD` is simply `$(CC)` on all hosts (`erts/configure.ac`), so `cc.sh` is also the linker driver and re-dispatches to `ld.sh`.
- **`emu_cc.sh` exists because `cl.exe` cannot compile computed gotos**: for `TARGET=win32` non-debug builds `erts/emulator/Makefile.in` uses `EMU_CC=@EMU_CC@` (the `emu_cc.sh` found in `PATH`) for `beam_emu.c`; it compiles with mingw `gcc` and then runs `coffix.exe` to make the object linkable by MSVC's linker. See `make/autoconf/otp.m4` (`LM_FIND_EMU_CC`).
- **`coffix.c`**: gcc/mingw objects put the virtual size in the COFF section's `PhysicalAddress` field with raw size 0, which confuses the MS linker when sizing `.bss`; `coffix -e` moves the value into `SizeOfRawData` so mixed gcc/MSVC objects link.
- **Three near-identical copies** (`cygwin_tools`, `../msys_tools`, `../wsl_tools`) instead of one script with runtime detection: each tree hardcodes its environment's path-conversion tool (`cygpath` vs `msys2win_path.sh`/`win2msys_path.sh` vs `wslpath`). The duplication is real — there is no shared include.

## Known Issues / Gotchas
- **Near-duplicate trees**: `../msys_tools/` (MSYS2) and `../wsl_tools/` (WSL) contain the same script set with different path translation (see `./CONTEXT.md` siblings). A functional change here almost always has to be mirrored into both, and consumed from `../../common/Makefile.in` / `otp_build` — check those callers before changing an interface.
- Observed divergences: `cygwin_tools` has both `vc/` and `mingw/`; `msys_tools` has only `vc/` plus `msys2win_path.sh`/`win2msys_path.sh`; `wsl_tools` has only `vc/` plus `SetupWSLcross.bat` (and its `w32_path.sh` is heavily rewritten around `wslpath`/`//wsl$` UNC paths). `msys_tools/vc/cc.sh` is the only variant that does *not* convert paths itself (it relies on MSYS2, plus `MSYS2_ARG_CONV_EXCL=-FoC`), and it also parses `Note: including file:` output for fast `-MM -MS` dependencies.
- Temp files are hardcoded to `/tmp` in several scripts (`reg_query.sh` batch file, `cc.sh`/`ld.sh`/`mc.sh`/`rc.sh` message files). Fine under Cygwin/MSYS2; the WSL variants instead use `$ERL_TOP/tmp`/`$ERL_TOP/tmpobj` because `/tmp` is not visible to `cl.exe`.
- `cc.sh` prints compiler output with `tail -n +2` / `tail +2` to drop Microsoft's banner line; a tool whose greeting spans more lines will leak text into `configure` tests.
- `cc.sh` refuses multiple source files combined with a single `-o *.o`.
- The `erl`/`erlc` wrappers re-`eval` the Windows executable, so the built `erl.exe`/`erlc.exe` must already be on `PATH`; they are launchers, not interpreters.
- `reg_query.sh` and the `+{preproc_flags,...}` handling in `erlc` are currently uncalled; do not assume they are exercised by CI.

## Notes for Agents
- To know whether a script is live, search the *callers*, not this directory: `otp_build` (functions `echo_env_cygwin`, `echo_env_msys`, `echo_env_wsl`, `echo_env_mingw32`), `make/autoconf/win*.config.cache.static`, `erts/configure.ac`, root `Makefile.in`, `erts/Makefile`, `erts/etc/common/Makefile.in`, `make/dep.mk`, `make/test_target_script.sh`.
- `otp_build env_win32 [x64|arm64|x64_arm64]` picks the tree at runtime: `/bin/wslpath` present → `../wsl_tools`, `/usr/bin/msys-?.0.dll` present → `../msys_tools`, otherwise this one; `otp_build env_mingw32` (described in `otp_build` as "Cygwin build without microsoft visual C++ (dead?)") uses the `mingw/` subdirectory and additionally requires `MINGW_EXE_PATH`/`WINE_EXE_PATH`.
- When mirroring a change, diff the three trees first (`diff -r erts/etc/win32/cygwin_tools erts/etc/win32/msys_tools`) — they drift intentionally, so copy semantics, not bytes.
- Design background for the whole Windows build is in `HOWTO/INSTALL-WIN32.md` (tool setup, `otp_build`, wrapper rationale).

## Dependencies
- `/bin/sh` (or bash), `sed`, `awk`, `perl` (dependency parsing in the MSYS/WSL variants), `cygpath` (Cygwin).
- Microsoft Visual Studio build tools on `PATH`: `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`, `reg.exe` (or `%WINDIR%\sysnative\reg.exe`).
- For `vc/emu_cc.sh`: a mingw gcc (e.g. `x86_64-w64-mingw32-gcc.exe` for `CONFIG_SUBTYPE=win64`).
- For `mingw/`: mingw `ar.exe`/`windres.exe`/gcc (`MINGW_EXE_PATH`) and wine's `wmc.exe`/`wrc.exe` (`WINE_EXE_PATH`).
- Only usable on a Windows host (Cygwin); nothing here runs on Linux/macOS.

## Test Strategy
- No automated tests: these scripts live outside any test framework and are exercised only by an actual Windows build (`eval \`./otp_build env_win32 x64\` && ./otp_build configure && ./otp_build boot -a`).
- Manual smoke checks: `javac.sh --help`-style invocations of `cc.sh`/`erl`/`erlc`, `w32_path.sh -m/-w/-d <path>`, and building `erts` with `TYPE=debug` (which bypasses `EMU_CC`/coffix).
- A Cygwin/MSYS2/WSL Windows CI job (`.github/`) is the only coverage for the translatation logic.

## Routing Table
- `vc/` → MSVC shims (`cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c`)
- `mingw/` → mingw/gcc variants of the same shims (used by `otp_build env_mingw32`)
- `../msys_tools/` → MSYS2 copy of this tree (sibling — read-only, mirror changes via this node's parent)
- `../wsl_tools/` → WSL copy of this tree (sibling — read-only, mirror changes via this node's parent)
- `../` → rest of `erts/etc/win32/` (`erlsrv/`, `nsis/`, `erl.c`, `Install*`, `win_erlexec.c`, …)
- `../../common/` → portable C sources and `Makefile.in` that consume these scripts (`RC=rc.sh`, `MC=mc.sh`, `USING_VC`)

## See Also
- `../../CONTEXT.md` (erts root), `../../../CONTEXT.md` (repo root)
- `../../common/Makefile.in` — main build-time consumer (`USING_VC`, `-MT`/`-MD`)
- `../erl.c`, `../init_file.c` — read the `bin/erl.ini` written by `make_*_ini.sh`
- `../../unix/` — the POSIX-only counterpart of `../` (no shims needed there)
- `otp_build` (repo root), `HOWTO/INSTALL-WIN32.md` (i.e. `../../../../HOWTO/INSTALL-WIN32.md`)
