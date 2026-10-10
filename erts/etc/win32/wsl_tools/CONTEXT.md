# erts/etc/win32/wsl_tools/ — WSL toolchain shims

## Intent
Bourne-shell wrappers that let a **WSL (Ubuntu) bash environment on a Windows host** drive the native Windows toolchain to build native win32 OTP. `otp_build env_win32` (auto-selected when `/bin/wslpath` exists) prepends this dir and `vc/` to `PATH`, exports `CONFIG_SUBTYPE` and `CC=cc.sh CXX=cc.sh AR=ar.sh RANLIB=true`, then configure/make run as on Unix; every tool call is translated to a Windows command.
This is *not* cross-compilation in the usual sense: the Windows compiler/linker (via WSL interop) produces the native binaries; only the emulator core is compiled by the Linux-side mingw-w64 gcc (`vc/emu_cc.sh`).
The user-facing description of this build mode is `../../../../HOWTO/INSTALL-WIN32.md`; the CI that exercises it is `.github/workflows/reusable-build-windows.yaml` (job runs with `shell: wsl-bash {0}`, installs `gcc-mingw-w64`, then `eval `./otp_build env_win32 x64``).

## API Surface
- `erl` — POSIX-sh stand-in for `bin/erl.exe`: rewrites `-I/-o` and absolute paths with `wslpath -m`, exports a Windows-form `ERL_TOP` through `WSLENV` (`ERL_TOP/w:...`), then `eval erl.exe $CMD`.
- `erlc` — same for `bin/erlc.exe`; also translates `-pa`, and `+'{preproc_flags,...}'` (recurses via `--echo_only`). `--echo_only` prints the translated command line instead of running it.
- `javac.sh` — same for `javac.exe` (used by wx); sets `WSLENV=CLASSPATH/l`.
- `w32_path.sh` — the WSL path translator, called by name from Makefiles/scripts. Modes: `-m` (Windows path with forward slashes), `-d` (**doubled** backslashes, for `erl.ini`/makefile use), `-w` (native backslashes), `-u` (Unix path; special-cases `\\wsl$\...` UNC input), `-a` (force absolute). A relative path without `-a` is converted textually, because current `wslpath` insists on absolute, existing paths.
- `make_local_ini.sh` — writes `$ERL_TOP/bin/erl.ini` (`Bindir=$ERL_TOP/bin/win32`) so the in-tree `bin/erl.exe` can find `erlexec.dll`; invoked by `erts/Makefile`'s win32 `local_setup` branch.
- `make_bootstrap_ini.sh <root> <bindir>` — same purpose for the bootstrap tree: writes `<root>/bin/erl.ini` pointing at an explicit `Bindir`; invoked by the top-level `Makefile.in` `bootstrap_setup` target (win32 branch) after copying `erl.exe`/`erlc.exe`/… into `bootstrap/bin`.
- `reg_query.sh <key> <valuename>` — reads a Windows registry value through `reg.exe` (view `/reg:64` for win64/arm64/x64_arm64, else `/reg:32`), strips the `REG_*` type prefix and normalises `\`→`/`. No in-tree caller was found — it is invoked by name from `PATH`.
- `SetupWSLcross.bat` — **unique to this tree**. Windows batch, run from WSL as `eval `cmd.exe /c erts/etc/win32/wsl_tools/SetupWSLcross.bat x64`` (or `x86`/`arm64`/`amd64_arm64`/`x64_arm64`). Locates `vcvarsall.bat` via `vswhere.exe`, calls it, then prints shell assignments for `INCLUDE`, `LIB`, `LIBPATH`, `VCToolsRedistDir`, `CL_PATH`, `RC_PATH`, `PATH` and `WSLENV` (echoed back through a nested `wsl.exe` so the values are re-translated into Unix form). `otp_build`'s `setup_win32_cl_env` evals that output.
- `vc/` — the MSVC compiler/linker shims (`cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c`); see the routing table.

## Constraints
- Requires bash/sh plus WSL interop executables on `PATH`: `wslpath`, `wsl.exe`, `cmd.exe`, `reg.exe`; and `w32_path.sh` (this dir) on `PATH`.
- Callers must have `ERL_TOP` and `CONFIG_SUBTYPE` (`win64`/`arm64`/`x64_arm64`, otherwise 32-bit x86) exported; `otp_build env_win32` does this.
- Path handling contracts: Windows tools need drive-letter paths — use `-m` (forward slashes, accepted by cl.exe) or `-w`/`-d` (backslashes); absolute paths are forced whenever the build tree is reached through the `//wsl$/…` UNC form, i.e. when it sits on the Linux filesystem rather than `C:\` (documented builds put the source on the Windows disk, `/mnt/c`).
- `erl.ini` backslashes must be doubled: the INI reader (`../init_file.c`) treats `\\` as one `\` and `\;` as a non-comment semicolon — this is why `-d` exists.
- Files the Windows tools create on `/mnt/c` are invisible to WSL for a moment and carry no Unix executable bit (see Known Issues).

## Design Decisions
- **Shims rather than native configure support**: `configure`/`make` already emit POSIX command lines, the Windows tools do not understand them. Minimal Bourne-shell wrappers translate the flags actually used; per `HOWTO/INSTALL-WIN32.md` they are deliberately "powerful enough to allow building Erlang/OTP, no more, no less", and shell (not Perl) so they are easy to patch.
- **Three near-identical trees instead of one env-detecting script**: `otp_build env_win32` detects `/bin/wslpath` (WSL), MSYS2, else Cygwin, and puts the matching tree on `PATH`; each tree hard-wires only its own shell's path helper (`wslpath`/`w32_path.sh` vs `cygpath` vs `msys2win_path.sh`). This is real duplication (e.g. two spellings of `erl.ini` in `w32_path.sh` comments), accepted because a wrapper must work using only its own shell's tools.
- **WSL-specific transport**: `wslpath` replaces `cygpath`/`msys*_path.sh`, and `WSLENV` is used to carry `ERL_TOP`, `CLASSPATH` and the MSVC `INCLUDE`/`LIB`/`LIBPATH` variables across the WSL↔Windows boundary.
- **The Linux-side mingw-w64 gcc is used only for the emulator core** (`vc/emu_cc.sh`), for the GCC labels-as-values speedup, with `coffix` fixing the resulting COFF for MSVC's linker; `mingw/` (a full mingw build tree) exists only under `cygwin_tools/`.

## Known Issues / Gotchas
- Windows Defender/Search can lock a just-created output, making the `mt.exe` manifest step fail; `vc/ld.sh` prints a hint and deletes the file.
- Windows tools do not set the Unix executable bit on files created on a Linux disk, so `vc/ld.sh` `chmod +x`es produced `.exe`s — configure would otherwise think the compiler failed (`test -x`).
- `/mnt/c` writes are not immediately visible to `wsl.exe`/`cmd.exe`; `vc/cc.sh` polls with `cmd.exe /C "IF EXIST ..."` until its temp object dir shows up.
- Newer `wslpath` refuses relative and non-existent paths, hence the textual fallback inside `w32_path.sh` and the `-a` option.
- `vc/cc.sh` must use absolute paths (`USEABSPATH`) when the build tree is reached through the `//wsl$/` UNC form, which happens in "unix" directories.
- `../cygwin_tools/` and `../msys_tools/` receive almost every fix made here and vice versa; a fix applied only here silently leaves the other two Windows build environments broken.

## Notes for Agents
- **Mirror changes**: this tree, `../cygwin_tools/` and `../msys_tools/` are near-duplicates; `../../common/Makefile.in` (and `otp_build`) consume them, so check those too when you rename a script or change its options.
- Which tree is used is decided in `otp_build` (`env_win32`/`env_win64`), not here.
- Optional debug tracing: set `CC_SH_DEBUG_LOG`, `LD_SH_DEBUG_LOG`, `MC_SH_DEBUG_LOG`, `RC_SH_DEBUG_LOG`, `EMU_CC_SH_DEBUG_LOG` to a file path to append the expanded command lines.
- In `.bat` files `%` must be written `%%` (batch `FOR` variable escaping); in `erl.ini` it is the *backslash*, not `%`, that is escaped.
- `CONFIG_SUBTYPE` selects machine targets (`win64`/`arm64`/`x64_arm64` vs 32-bit) in `SetupWSLcross.bat` callers, `vc/cc.sh`, `vc/emu_cc.sh` and `reg_query.sh`.
- Non-WSL launchers live in `../` and `../../common/`; do not confuse this tree with the unix scripts in `../../unix/`.

## Routing Table
- `vc/` → MSVC compiler/linker/assembler/resource/message shims (`cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c`)
- `../cygwin_tools/` → near-duplicate shim tree for Cygwin (adds `mingw/`) — read-only, escalate writes to `../`
- `../msys_tools/` → near-duplicate shim tree for MSYS2 (`msys2win_path.sh`, `win2msys_path.sh`) — read-only, escalate writes to `../`
- `../../common/Makefile.in` → consumer of `cc.sh`/`ld.sh`/`ar.sh`/`rc.sh`/`mc.sh` (`USING_VC=@MIXED_VC@`) — read-only, escalate writes to `../../`
- `../../../../Makefile.in` → `bootstrap_setup` target calling `make_bootstrap_ini.sh`; `erts/Makefile` → `local_setup` calling `make_local_ini.sh`

## Dependencies
- WSL 1 or 2 with Ubuntu (bash, sed, perl, `wslpath`).
- Microsoft Visual Studio (2019+): `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`, found via `vswhere`/`vcvarsall`.
- Linux-side `gcc-mingw-w64` / `g++-mingw-w64` cross toolchain for `vc/emu_cc.sh`.
- Runtime helpers on this `PATH`: `w32_path.sh`, `make_local_ini.sh`, `make_bootstrap_ini.sh`.

## Test Strategy
- None automated — these are build-tooling shims with no unit tests. They are validated only by a real WSL+MSVC build (`./otp_build env_win32 x64 && ./otp_build configure && ./otp_build boot -a`), as done in `.github/workflows/reusable-build-windows.yaml`. Debugging is manual, via the `*_SH_DEBUG_LOG` variables.

## See Also
- `../CONTEXT.md` — Windows-specific launchers/sources that these shims build (`erl.c`, `erlsrv/`, `nsis/`, `Install.c`)
- `../../CONTEXT.md` — `erts/etc/` split into `common/`, `unix/`, `win32/`
- `../../../../HOWTO/INSTALL-WIN32.md` — the documented WSL build procedure and FAQ
- `../../../../otp_build` — `env_win32`/`env_win64` environment setup and tree selection
- `../../../../.github/workflows/reusable-build-windows.yaml` — CI usage of this build mode
