# erts/etc/win32/ — Windows platform half of `erts/etc/` (launchers, resources, installer)

## Intent
Windows-only counterpart of `erts/etc/unix/`: the C sources, resource scripts and installer parts needed to run ERTS on Windows.
`erts/etc/` selects its platform subdirectory in `erts/etc/Makefile` (`SUB_DIRECTORIES = common` + `win32` on `TARGET=win32`, else `unix`), so this directory is visited by the ordinary `make` in `erts/etc`.
Almost no compilation happens here: the portable C sources live in `../common/` and the compile/link rules for the Windows launchers are in `../common/Makefile.in` (which adds `-I$(WINETC)`, `RC=rc.sh`, `MC=mc.sh` and includes `erlsrv/*.h`).
This directory's own `Makefile` is a no-op for `opt/debug/all/clean` and only implements `release_spec`.
Sibling top-level areas: `../../` (ERTS as a whole), `../../../lib/` (OTP applications).

## API Surface
### What is built from here (all compiled and linked by `../common/Makefile.in`, output in `$ERL_TOP/bin/win32/`)
- `erl.exe` ← `erl.c` + `init_file.o` + `erl.res`. Small loader: reads `erl.ini`, loads `erlexec.dll`, converts `wchar_t` argv to UTF-8 and calls the DLL's `win_erlexec` export.
- `erlexec.dll` ← `../common/erlexec.c` + `win_erlexec.c` + `init_file.o` + `erl.res` (`ERLEXEC = erlexec.dll` on win32, `erlexec` elsewhere). The real launcher/argument processor; a DLL, not an exe.
- `erl_log.exe` ← `erl_log.c`. Echoes stdin to `CONOUT$`.
- `Install.exe` ← `Install.c` + `init_file.o`, together with the generated `Install.ini`. Installed at the *release root* (not `bin/`), run by the installer/patch flow to copy `erts-<vsn>/bin/*` into `<root>/bin`, create the `werl.exe` link and select the default `start.boot`.
- `start_erl.exe` ← `start_erl.c` (Windows counterpart of the `../unix/start_erl.src` shell script): reads `start_erl.data`, `releases/`, `bootflags`, the `Software\Ericsson\Erlang\...` registry key, and spawns the emulator.
- `erlsrv.exe` ← sources in `erlsrv/` (see routing table).
- `port_entry.o` (`erl_port_entry` entry point) from `port_entry.c`, linked into `heart.exe` and `inet_gethost.exe` with `-entry:erl_port_entry`; it ignores `CTRL_LOGOFF_EVENT`/`CTRL_SHUTDOWN_EVENT` when `ERLSRV_SERVICE_NAME` is set, so a service-hosted node is not killed on logoff.
- Resource objects: `erl.res` (from `erl.rc`), `erlsrv_logmess.res`/`.o` (from `erlsrv/erlsrv_logmess.mc`), and the emulator's preload resource `beams.res`/`beams.o` — the emulator Makefile compiles the generated `beams.rc` with `-I$(ERL_TOP)/erts/etc/win32`, which is how the `#include <beam.rc>` inside it resolves to this directory.
- `version.h` — **generated into this directory** from `version.h.src` by `../common/Makefile.in`; git-ignored by `erts/.gitignore` (`/etc/win32/version.h`).

### Release installation (`Makefile` `release_spec`)
- Installs the already-built `$(ERL_TOP)/bin/$(TARGET)/{inet_gethost,erl,heart,erlc,erlsrv,start_erl}.exe` into `release/win32/erts-$(VSN)/bin`.
- Installs `start_erl.c` + `Nmakefile.start_erl` into `erts-$(VSN)/src` so `start_erl.exe` can be rebuilt standalone with `nmake` without the full OTP build.
- Installs `beam_icon.ico`, `erl_icon.ico`, `hrl_icon.ico` into `release/win32/usr/lib/icons`, and creates the empty `usr/include` / `usr/lib` directories.
- The *full* program list shipped in `erts-$(VSN)/bin` (escript/dialyzer/typer/ct_run/erl_call/erlexec.dll + `.pdb` files) and the release-root `Install.exe`/`Install.ini` come from `../common/Makefile.in`'s `release_spec`, which targets the same directory.

### The `erl.ini` mechanism (Windows replacement for the Unix `erl` shell-script logic)
- `erl.ini` holds section `[erlang]`, key `Bindir` (UTF-8) — the directory containing `erlexec.dll`.
- Readers: `erl.c`, `../common/erlexec.c` (the DLL itself) and `erts/preloaded/src/init.erl` (`Bindir`).
- If `erl.ini` is absent, `erl.c` falls back to `find_erlexec_dir()`: it derives the directory from its own path, and if that is not `erts-<vsn>/bin` it scans for the newest `erts-*` sibling and exports `ERLEXEC_DIR`.
- Installed trees therefore ship **without** `erl.ini` (relocatable install). It is written only where deduction fails: `$ERL_TOP/bin/erl.ini` (+ copy in `bin/win32/`) during a source build, the bootstrap tree's `bin/erl.ini` during a bootstrap build, and at upgrade time on the target by `lib/sasl/src/release_handler.erl` / `lib/reltool/src/reltool_target.erl` (which document that other C files reading `erl.ini` must stay in sync).
- `init_file.c`/`init_file.h` are the parser/writer for these files and are also reused by `Install.c` (for `Install.ini`).
- Quoting rule: `init_file.c` unescapes `\\` and `\;`, so `Bindir` values written by the toolchain shims escape every backslash.

### Template files (never edit the generated output)
- `version.h.src` → `version.h`: `%ERTS_VERSION%`, `%ERTS_VERSION_INTS%`, `%OTP_VERSION%`, `%OTP_VERSION_INTS%`, `%YEAR%`. The `_INTS` forms are the version truncated to the first 4 dot-separated numbers, comma-joined, with `OTP_VERSION` first stripped at the first `-` (so `30.0-rc0` → `30,0`); they feed `FILEVERSION`/`PRODUCTVERSION` in `erl.rc`.
- `Install.src` → `Install.ini` at build time (`%I_VSN%` = erts VSN, `%I_SYSTEM_VSN%`).
- `Nmakefile.start_erl` — shipped example `nmake` file for `start_erl.exe`.
- `INSTALL.txt` — shipped verbatim in the Windows zip (`upload-windows-zip.yaml`) telling users to install the MSVC redistributable; mirrors what the NSIS installer bundles.

### Resources and the `.rc`/`.mc` pipeline
- `erl.rc` — the `erl.exe`/`erlexec.dll` resource: `erlang.ico` as icon 1 + `VS_VERSION_INFO` built from the generated `version.h`.
- `beam.rc` — a resource *fragment* (not compilable on its own), `#include`d by the generated emulator `beams.rc`: icons 1–4 (`erlang.ico`, `erl_icon.ico`, `hrl_icon.ico`, `beam_icon.ico`), `toolbar.bmp`, a menu, an "About Erlang Shell" dialog and accelerators.
- `resource.h` — ids used by `erl.rc`/`beam.rc` (`IDR_CONMENU`, `IDMENU_*`, `ID_*_VERSIONSTRING`, `ID_BREAK`, `ID_COMBOBOX`).
- `manifest.xml` — the default assembly manifest embedded into Windows binaries by the WSL toolchain shim (`wsl_tools/vc/ld.sh`) when `link.exe` produces none: Common-Controls 6.0 dependency, an explicit `supportedOS` list, and `AsInvoker` (disables UAC installer-detection/virtualisation). The cygwin/msys `ld.sh` only patch the linker-generated manifest and do not use this fallback.
- `.rc` files are compiled by `rc.sh` and `.mc` files by `mc.sh` (both `RC`/`MC` set in `../common/Makefile.in`); the actual compilers are MSVC `rc.exe`/`mc.exe` or, on the mingw path, `windres.exe`/`wine-wmc.exe`.

### Installer
`nsis/` builds the official Windows setup `.exe` from an already-built release tree; `manifest.xml`, `INSTALL.txt` and the icons feed into it.

## Constraints
- Tools used: MSVC (`cl`, `link`, `lib`, `rc`, `mc`, `mt`) or the mingw-w64 equivalents; the build driver is `otp_build env_win32` (or `env_mingw32`), **not** anything in this directory.
- `../common/Makefile.in` sets `USING_VC=@MIXED_VC@`. With `USING_VC=yes` the erlsrv objects get `-MT` and the message catalog links as `.res`; with `no` they get `-MD`, the `.res` is converted to a `.o` by `rc.sh`, and `ws2_32.lib mswsock.lib` becomes `-lws2_32`.
- The `mc`/`rc` steps are deliberately serialised on Windows by false dependencies on `$(OBJDIR)/LOGMESS-GENERATED`, because the MSVC message/resource compilers transiently create and delete a top-level `vc100.pdb` that confuses ordinary compilations running in parallel.
- Everything user-visible is wide-char (`wmain`, `wchar_t`), with UTF-8 conversion at the boundary (`erl.c`, `win_erlexec.c`); non-ASCII paths and arguments must work.
- Files here include each other by name (`init_file.h`) and are compiled with `-I` at this directory — individual files cannot be relocated without touching `../common/Makefile.in`.
- Do not edit the generated `version.h`; edit `version.h.src`. Do not edit `Makefile` rules expecting them to build anything.
- `erl.c`'s fallback search assumes it is not a symbolic link and the path is absolute; `install` paths may require administrator rights for link creation (`Install.c` falls back to copying).

## Design Decisions
- **`erl.exe` + `erl.ini` instead of a shell script**: Windows has no usable `sh`, so argument quoting, path mangling and `ERL_*` variable setup live in C, and the location of the runtime is supplied by an INI file rather than Unix environment variables/relative shebang tricks.
- **`erlexec` is a DLL on Windows**: it lets one launcher implementation be shared by `erl.exe`/`werl.exe` and be invoked in-process; `win_erlexec.c` either loads the emulator DLL (`ERL_EMULATOR_DLL`, `load_win_beam_dll` → `erl_start`) or, when there is no console, re-spawns itself detached with `ERL_CONSOLE_MODE=detached`.
- **`werl.exe` is not compiled**: `Install.c` creates it as a link to `erl.exe` (or copies it when the link cannot be created).
- **`erl.ini` is not installed**: the installed tree is meant to be relocatable, so `erl.exe` deduces the erts directory when the INI file is missing.
- **Resources are shared fragments**: `beam.rc` is `#include`d rather than duplicated so the emulator and the launcher share icons; icons for Explorer/installer use are also released under `usr/lib/icons`.
- **`port_entry.c` overrides the CRT entry point** (`-entry:erl_port_entry`) rather than wrapping `main`, so port programs survive logoff/shutdown when hosted by a service.
- **Virtualisation is switched off centrally**: the `AsInvoker` trust info is injected by the linker shims (and provided as `manifest.xml`), so every OTP binary gets consistent UAC behaviour.

## Known Issues / Gotchas
- `resource.h`'s menu/dialog/accelerator ids and the MENU/DIALOG/ACCELERATORS blocks in `beam.rc` have no C consumer anywhere in the tree (only the icon/bitmap resources are meaningful now); nothing should be inferred from their presence.
- `Makefile` defines `INSTALL_LIBS = $(BINDIR)/erl_dll.lib` but `release_spec` never uses it, so the emulator import library is not shipped by this file; `usr/lib` is created empty and `usr/lib/icons` is populated with no in-tree consumer.
- `Makefile`'s `INSTALL_PROGS` is a legacy subset (`inet_gethost`, `erl`, `heart`, `erlc`, `erlsrv`, `start_erl`); `escript.exe`, `erl_log.exe` and `erlexec.dll` are only contributed by `../common/Makefile.in`.
- `version.h` is absent in a clean checkout — `erl.rc` cannot be compiled until `../common/Makefile.in` has generated it.
- `lib/kernel/test/os_SUITE.erl` asserts that `os:version()` matches `cmd /c ver`, explicitly warning that a new Windows release requires updating `erts/etc/win32/manifest.xml`.
- Cross-tree divergence: only `wsl_tools/vc/ld.sh` falls back to `manifest.xml`; a manifest problem seen under Cygwin/MSYS2 has a different cause.

## Notes for Agents
- To compile anything here, run `make` in `erts/etc/common` from a **configured** win32 tree; running `make` in this directory only reaches `release_spec`.
- Read the child `CONTEXT.md` files before touching a `*_tools` shim: the three trees (`cygwin_tools/`, `msys_tools/`, `wsl_tools/`) are near-duplicates selected at `otp_build env_win32` time, and a functional change normally has to be mirrored into all three (and re-checked against `../common/Makefile.in` and `../../../otp_build`).
- `../common/Makefile.in` is the single place where the C sources of this directory are wired into the build — search there first when a source file here appears unused.
- `../../../../HOWTO/INSTALL-WIN32.md` is the authoritative description of the three toolchain environments; `HOWTO/INSTALL-WIN32-OLD.md` covers cygwin/msys only.
- Erlang-side consumers of the artifacts defined here: `lib/sasl/src/erlsrv.erl`, `lib/sasl/src/release_handler.erl`, `lib/reltool/src/reltool_target.erl`, `erts/emulator/sys/win32/sys.c` (`erl_log.exe`, `-service_event`).

## Dependencies
- Windows SDK / MSVC toolchain (`cl`, `link`, `lib`, `rc`, `mc`, `mt`) or mingw-w64 (`gcc`, `windres`, `wine-wmc`) plus the shims under `*_tools/`.
- GNU make plus a bash-compatible host (Cygwin, MSYS2 or WSL) for the build itself; `otp_build env_win32` picks the matching shim tree.
- NSIS ≥ 3.0.6 for the installer (`nsis/`).
- Registry and service APIs (`advapi32`) for `erlsrv`/`start_erl`; `ws2_32`/`mswsock` for `inet_gethost`/`heart`.
- Documentation sources `erts/doc/references/{erl,werl,erlc,escript,erlsrv,start_erl}_cmd.md` describe the shipped CLIs.

## Test Strategy
- `erts/test/nt_SUITE.erl` is the only suite covering `erlsrv.exe`; note that `all/0` schedules the real service tests only when `os:version() =< {6,1,999999}` (Windows 7 and older) and falls back to just `[nt]` on newer Windows, so modern CI exercises very little.
- `erts/test/install_SUITE.erl` skips entirely on `{win32,_}` ("Not on windows").
- Cross-checks elsewhere: `lib/sasl/test/systools_SUITE.erl` asserts `erlexec.dll`/`erl_log.exe` are present in release file lists; `lib/sasl/test/release_handler_SUITE.erl` covers `erl.ini` rewriting on upgrade (fixture `release_handler_SUITE_data/erl.ini.src`); `lib/kernel/test/os_SUITE.erl` ties `manifest.xml` to `os:version()`; `lib/kernel/test/heart_SUITE.erl` uses `werl`.
- Real Windows builds run only in CI: `.github/workflows/reusable-build-windows.yaml` (WSL → `otp_build env_win32 x64` → build → `otp_build installer_win32`, NSIS installed via choco) and `upload-windows-zip.yaml` (copies `INSTALL.txt` into the zip).
- Coverage gaps: nothing automated validates the installed tree, the registry/Start-menu/association entries, `erl.ini` handling, or the installer/uninstaller.

## Routing Table
- `erlsrv/` → `erlsrv.exe`, the Windows service wrapper and service CLI (registry config, SCM integration, message catalog `erlsrv_logmess.mc`); source of the `erlsrv_logmess.res/.o` resource
- `cygwin_tools/` → Cygwin bash shims that drive MSVC (and mingw) for `TARGET=win32`; unique in having both `vc/` and `mingw/` variants
- `msys_tools/` → MSYS2 bash shims driving MSVC; only `vc/`, with its own `msys2win_path.sh`/`win2msys_path.sh` path converters
- `wsl_tools/` → WSL bash shims driving MSVC; only `vc/`, plus `SetupWSLcross.bat` and the `manifest.xml` fallback in `vc/ld.sh`
- `nsis/` → NSIS installer build (`erlang20.nsi`, `release_spec`, version/redist helper scripts); run only by `otp_build installer_win32`
- `../common/` → the portable launcher C sources this directory complements; `../common/Makefile.in` is what compiles everything here (sibling — read-only, escalate writes to `../`)
- `../unix/` → the Unix counterparts (`erl.src.src`, `start_erl.src`, `run_erl`, `to_erl`, `Install.src`) (sibling — read-only)
- `../../test/nt_SUITE.erl`, `../../test/install_SUITE.erl` → the Windows/install test suites (outside this node — read-only)
- `../../emulator/utils/make_preload` + `../../emulator/Makefile.in` → generate `beams.rc` that `#include`s `beam.rc` from here (outside this node — read-only)
- `../../../../HOWTO/INSTALL-WIN32.md` → describes the three toolchain environments and the Windows build procedure (outside this node — read-only)

## Status
Top-level files and all five child directories are documented.
`../CONTEXT.md` (`erts/etc/`) is still empty and is owned by the parent node.
