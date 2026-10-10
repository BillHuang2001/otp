# erts/etc/win32/wsl_tools/vc/ — MSVC + mingw toolchain shims (WSL)

## Intent
The compiler-side half of the WSL Windows build support: Bourne-shell wrappers that present the Microsoft Visual C++ tools (`cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`) to autoconf-generated Makefiles as if they were Unix `cc`/`ld`/`ar`/`rc`/`mc`. Plus `emu_cc.sh`/`coffix`, which let the Linux-side mingw-w64 gcc compile the emulator core objects for the MSVC linker.
The non-compiler WSL shims (`erl`, `erlc`, `javac.sh`, `w32_path.sh`, `make_local_ini.sh`, `make_bootstrap_ini.sh`, `reg_query.sh`, `SetupWSLcross.bat`) live in `../`.
Used only for a **native win32 build driven from WSL**; the Windows compiler runs via WSL interop, so this is not classical cross-compilation (except for the emulator core, see `emu_cc.sh`).
Invoked by name from the build: `otp_build`'s `env_win32` prepends `erts/etc/win32/wsl_tools/vc` and `erts/etc/win32/wsl_tools` to `PATH` and exports `CC=cc.sh CXX=cc.sh AR=ar.sh RANLIB=true`.

## API Surface
Unix flag in → MSVC flag out. Each script is a translator whose contract is "accept the exact command lines our Makefiles/configure emit, no more".

- `cc.sh` — the `cl.exe` wrapper and the tree's entry point; also calls `ld.sh` when not `-c`. Maps `-I`→`/I`, `-D`/`-EH*`/`-TP` passed through, `-o`→object/exe name, `-O*`→`-Ox -Z7`, `-g`→`-Z7`, `-Werror`→`/WX`, `.cpp`→`/EHsc`, `-Owx`→the wxErlang flag set; chooses the stdlib (`-mt|-MT`→`-MT`, `-md|-MD`→`-MD`, `-ml|-ML`, `-mdd|-MDD` and `-d` variants; default `-MD`, `-g` without optimization switches to `-MDd`) and passes the matching `-lXXX` to `ld.sh`; `-E` = preprocess-only stdout, `-MS` = preprocess + emit make dependencies (perl over the `#line` comments), `-MM` = **fast dependency** mode (single `cl.exe /showIncludes` for all sources + one perl pass); compiles each source into `$ERL_TOP/tmpobj$$` when linking, or to `-o` target when `-c`.
- `ld.sh` — the `link.exe` wrapper (also reachable directly; `make/autoconf/otp.m4` forces `LD=ld.sh` on Windows). `-l<name>`→`<name>.lib` (via `w32_path.sh`), `-L`→`-libpath:`, `-dll`, `-o`→`-out:`, `-def:`/`-implib:`, `-g`→`-debug`; picks the CRT by the `-lMSVCRT*/-lLIBCMT*` flag it was handed; always links the default libs `kernel32 advapi32 gdi32 user32 comctl32 comdlg32 shell32` (+ `WS2_32 IPHLPAPI` when `-lsocket` was seen); `-nologo -incremental:no -MACHINE:<x64|ARM64|x86>` (machine comes from `CONFIG_SUBTYPE` in `cc.sh`); generates the `.pdb`; post-processes/embeds the manifest via `mt.exe` (`../../manifest.xml` when the linker produced none) and `chmod +x`es the produced `.exe`.
- `ar.sh` — `lib.exe` wrapper (`-out:` handling, `/nologo`).
- `rc.sh` — `rc.exe` wrapper; locates a genuine MS `rc.exe` by probing each `PATH` entry's usage text for "resource compiler", writes output via `-Fo`.
- `mc.sh` — `mc.exe` wrapper; same `PATH`-probing trick ("message compiler"), collects `-o <dir>` and `cd`s there before running (MS `mc.exe` writes relative to CWD).
- `emu_cc.sh` — compiles the **emulator core** sources with the Linux mingw-w64 cross gcc (`x86_64-w64-mingw32-gcc -m64` for `win64`, `aarch64-w64-mingw32-gcc -m64` for `arm64`/`x64_arm64`, `x86_64-w64-mingw32-gcc -m32` otherwise), always with `-D__WIN32__ -DWIN32 -DWINDOWS -fomit-frame-pointer`, to a temp file, then runs `coffix.exe -e` on the object and copies it to `-o`; forces `-fno-tree-copyrename` and skips coffix in fast-dependency (`-MM`) mode. Also lazily (re)builds `coffix.exe` with `cl.exe` if it is older than `coffix.c`. Selected as `EMU_CC` by `../../../../../make/autoconf/otp.m4` (`LM_FIND_EMU_CC` searches `PATH` for `emu_cc.sh`), consumed via `EMU_CC`/`V_EMU_CC` in `../../../../emulator/Makefile.in`.
- `coffix.c` — small C tool (built by `cc.sh`-driven `cl.exe` into `coffix.exe`) that rewrites PE/COFF section headers so MSVC's `link.exe` accepts mingw-gcc objects: when `Misc.PhysicalAddress != 0` and `SizeOfRawData == 0` (the `.bss` case) it moves the virtual size into `SizeOfRawData` and zeroes `PhysicalAddress`. `-e` edits in place, `-v` prints; without `-e` it only diagnoses.

## Constraints
- POSIX `sh`/bash plus these executables on `PATH`: `w32_path.sh` (from `../`), `wslpath`, `cmd.exe`, `wsl.exe`, MSVC `cl.exe`/`link.exe`/`lib.exe`/`rc.exe`/`mc.exe`/`mt.exe`.
- `ERL_TOP` must be set (temp dirs `$ERL_TOP/tmpobj$$`, `$ERL_TOP/tmp`); `CONFIG_SUBTYPE` (`win64`/`arm64`/`x64_arm64`, else 32-bit x86) selects the target machine and cross gcc.
- Paths handed to Windows tools must be Windows-style: `w32_path.sh -d` (doubled backslashes, safe inside cl/link command lines) is the default, `-a` forces absolute. Relative Unix paths are only passed through when `USEABSPATH=false`.
- `USEABSPATH` is set per-invocation by testing `w32_path.sh -m $PWD` against `//wsl$/*` — i.e. when the build tree is reached through the `\\wsl$\...` UNC view rather than a `C:\` mount.
- No state beyond `/tmp/*.$$` scratch files; scripts must stay dependency-free (no Perl-only features beyond what WSL ships, no helper binaries beyond `coffix`).
- Keep the wrapper surface exactly as wide as the build needs — do not generalize (see Design Decisions).

## Design Decisions
- **Shell wrappers instead of teaching configure about MSVC**: `configure`/`make` emit POSIX command lines the Windows tools cannot parse. Minimal translators handle the flags actually used. `HOWTO/INSTALL-WIN32.md` states the intent explicitly — the scripts are "powerful enough to allow building Erlang/OTP, no more, no less" — and are written in shell rather than Perl precisely so they stay easy to patch.
- **`ld.sh` keeps its own Unix-flag parser** and is callable directly (not only through `cc.sh`), because configure probes the linker separately.
- **Linux mingw-w64 gcc only for the emulator core** (`emu_cc.sh`): GCC's labels-as-values computed goto gives the BEAM interpreter a large speedup that MSVC cannot reach; the resulting COFF objects are then made acceptable to `link.exe` by `coffix` (see `emu_cc.sh`/`coffix.c` above). Everything else is compiled by `cl.exe`.
- **Three near-duplicate trees, not one env-detecting script**: `otp_build env_win32` detects `/bin/wslpath` (WSL), MSYS2, else Cygwin and puts the matching `vc/` on `PATH`; each tree hard-wires only its own shell's path helper (`w32_path.sh`/`wslpath` vs `cygpath` vs `msys2win_path.sh`). The duplication is accepted because a wrapper must work with only its own environment's tools.
- **WSL-specific fast dependencies**: this tree's `cc.sh` has a `-MM` mode that runs one `cl.exe /showIncludes` for all sources and post-processes it with inline Perl, translating paths back to Unix with `wslpath -u`. The Cygwin/MSYS2 trees have no equivalent (they treat `-MM` exactly like `-MS`, i.e. per-file perl over `#line` comments).

## Known Issues / Gotchas
- Windows Defender or Windows Search can lock a just-created binary, making the `mt.exe` manifest step fail; `ld.sh` prints a hint about it and deletes the file.
- Windows tools do not set the Unix executable bit on files created on a Linux disk, so `ld.sh` `chmod +x`es produced `.exe`s — otherwise configure's `test -x` concludes the compiler failed. It also warns (but continues) when the `-dll`/`.exe` output extension contradicts the mode.
- `/mnt/c` writes are not immediately visible to `cmd.exe`, so `cc.sh` polls `cmd.exe /C "IF EXIST <tmpobjdir>"` until the Windows side sees the freshly made temp dir.
- Newer `wslpath` refuses relative and non-existent paths, hence `w32_path.sh`'s textual fallback and its `-a` mode; `cc.sh` therefore forces absolute paths under `//wsl$/*`.
- `mc.sh`/`rc.sh` scan `PATH` for a real Microsoft tool and *fail hard* on a same-named impostor (e.g. a `rc.exe` that is not the MS one) instead of skipping it.
- The `-MTd` stdlib maps to `-lLIBMTD` in `cc.sh` (`ld.sh` does accept the correct `-lLIBCMTD`); the `cc.sh` spelling would be a wrong lib name if that combination is ever used.
- Generated `coffix.exe` is ignored by the repo-root `.gitignore` only for `cygwin_tools/vc` and `wsl_tools/vc` — a `coffix.exe` built under `msys_tools/vc` is untracked-but-unignored.

## Notes for Agents
- **Mirror changes**: `../../cygwin_tools/vc/` and `../../msys_tools/vc/` are near-duplicates of this directory (same scripts, path helper swapped). A fix applied only here silently leaves the other two Windows build environments broken; check `../../../common/Makefile.in`, `../../../../../otp_build` and `../../../../../HOWTO/INSTALL-WIN32.md` too.
- Observed divergence to keep in mind: `../../cygwin_tools/vc/` additionally ships a whole extra `../../cygwin_tools/mingw/` build tree with its own `emu_cc.sh`; `wsl_tools/vc/` and `msys_tools/vc/` have neither. Only `wsl_tools/vc/cc.sh` has the fast-dependency path.
- Debug tracing: set `CC_SH_DEBUG_LOG`, `LD_SH_DEBUG_LOG`, `MC_SH_DEBUG_LOG`, `RC_SH_DEBUG_LOG`, `EMU_CC_SH_DEBUG_LOG` to a file path to append the expanded command lines (the first thing to do when a build fails inside a shim).
- Which tree is selected is decided in `otp_build` (`env_win32`/`env_win64`/`echo_env_wsl`), not here.
- Nothing in this directory is generated or auto-updated (unlike `../../../../autoconf/`); every file here is hand-maintained. `coffix.exe` is the only build product.
- Do not confuse this tree with the Unix launchers in `../../../unix/` or the shared Windows sources in `../../`.
- Flat directory: no subdirectories to route into. Work on the sibling trees belongs to `../../cygwin_tools/vc/` / `../../msys_tools/vc/` (read-only from here — escalate such writes to `../../`).

## Dependencies
- Microsoft Visual Studio (2019+) tools: `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe` (located by `SetupWSLcross.bat`/`vcvarsall` via `vswhere`).
- Linux-side `gcc-mingw-w64` / `g++-mingw-w64` for `emu_cc.sh` (`sudo apt-get install gcc-mingw-w64`).
- WSL 1 or 2 (Ubuntu) with bash, sed, perl (`perl` is required for `-MM`/`-MS` dependency generation) and `wslpath`.
- `../../manifest.xml` — the default manifest embedded by `ld.sh` when the linker emitted none.

## Test Strategy
- None automated and none possible off-Windows: these shims have no unit tests and are exercised only by a real WSL+MSVC build (`./otp_build env_win32 x64 && ./otp_build configure && ./otp_build boot -a`).
- CI: `.github/workflows/reusable-build-windows.yaml` (jobs run `shell: wsl-bash {0}`, install `gcc-mingw-w64`, then the same `otp_build` sequence).
- Debugging is manual, via the `*_SH_DEBUG_LOG` variables and the `/tmp/{cl,link,rc,mc}.exe.<pid>.{1,2}` capture files.

## See Also
- `../CONTEXT.md` — the WSL launcher/path/INI shims that put this directory on `PATH` and supply `w32_path.sh`
- `../../../CONTEXT.md` — `erts/etc/` (its `common/`, `unix/`, `win32/` split)
- `../../../common/Makefile.in` — the consumer that sets `CC`/`LD`/`RC`/`MC` and `USING_VC` (`@MIXED_VC@`), choosing `-MT` vs `-MD` and `.res` vs `.o` message output
- `../../../../../otp_build` — `env_win32`/`env_win64` environment setup and tree selection
- `../../../../../make/autoconf/otp.m4` — `LM_FIND_EMU_CC` (`emu_cc.sh` discovery) and `LD=ld.sh`
- `../../../../emulator/Makefile.in` — `EMU_CC`/`V_EMU_CC` consumption for the emulator core
- `../../../../../HOWTO/INSTALL-WIN32.md` — the documented WSL build procedure and the rationale for minimal wrappers
- `../../../../../.github/workflows/reusable-build-windows.yaml` — CI that exercises this build mode
