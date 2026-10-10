# erts/etc/win32/msys_tools/vc/ — MSVC shims (MSYS2)

## Intent
The `vc/` half of `../` (the MSYS2 shim tree): bash wrappers that make the Microsoft Visual C++
command-line tools usable from the OTP Unix-style build (make + `configure`) on a Windows host.
`otp_build env_win32` puts this directory first on `PATH` and sets `CC=cc.sh`, `CXX=cc.sh`, `AR=ar.sh`.
Only `cc.sh`/`ld.sh`/`ar.sh`/`emu_cc.sh` are found via `CC`/`AR`/`EMU_CC`; `rc.sh`/`mc.sh` are selected by
name in `erts/etc/common/Makefile.in` (win32 branch).
This is the `vc/` variant used by the MSYS2 host; sibling near-duplicates live in
`../../cygwin_tools/vc/` (plus `../../cygwin_tools/mingw/`) and `../../wsl_tools/vc/`.

## API Surface
- `cc.sh` — the "cc" wrapper for `cl.exe`. Parses Unix-ish flags: `-c`, `-E`, `-MM` (dependencies),
  `-O*` / `-O0` / `-Og`, `-g`, `-MT/-MD/-ML` (+`d`) stdlib, `-o`, `-I`, `-D`, `-l`, `-EH*`, `-TP`; collects
  `*.c/*.cc/*.cpp` sources and `*.o` inputs. When only compiling it emits an object (honoring `-o` as file or
  dir); when linking it compiles sources into a temp objdir and calls `ld.sh`. `-MM`/`-E` produce
  dependencies/preprocessed output (deps parsed from `#line` markers in a Perl one-liner). Exports
  `MSYS2_ARG_CONV_EXCL=-FoC` and adds `-D__WIN32__ -DWIN32 -DWINDOWS …`.
- `ld.sh` — the "ld" wrapper for `link.exe`. Handles `-dll`, `-L<dir>`→`-libpath:"…"`, `-l<name>`→`"<name>.lib"`
  (with `MSVCRT`/`LIBCMT*`/`socket` mapping to the right stdlib/`WS2_32`+`IPHLPAPI`), `-o`→`-out:`,
  `-implib:`, `-def:`, `-g`. Picks default libs (`kernel32/advapi32/gdi32/user32/comctl32/comdlg32/shell32`),
  emits a `.pdb`, and after linking injects a manifest with `mt.exe` adding a `trustInfo`/`AsInvoker`
  `requestedExecutionLevel`, then `chmod +x`es the output (Cygwin/Windows-7/Samba workaround carried over).
- `ar.sh` — the "ar" wrapper for `lib.exe` (static libraries). Translates `-out:` and input paths.
- `rc.sh` — resource-compiler wrapper. Because several `rc.exe` exist on Windows, it scans `PATH` for one
  whose `-?` output mentions "resource compiler", then runs it with `-Fo` output and translated `-I`/paths;
  exports `MSYS2_ARG_CONV_EXCL=-Fo`.
- `mc.sh` — message-compiler wrapper. Similarly locates the real `mc.exe`, `cd`s into the `-o` output dir,
  translates `-I`/input paths; exports `MSYS2_ARG_CONV_EXCL=`.
- `emu_cc.sh` — the **emulator** C compiler. Uses mingw-w64 gcc (`x86_64-w64-mingw32-gcc.exe` when
  `CONFIG_SUBTYPE=win64`, else `gcc`) to compile with `-D__WIN32__ -DWIN32 -DWINDOWS -fomit-frame-pointer`,
  then runs `coffix.exe -e` on the object so MSVC `link.exe` accepts it. Compiles `coffix.exe` on demand
  (cl.exe) when it is older than `coffix.c`. `-MM` mode skips coffix. Selected via `EMU_CC`
  (`make/autoconf/otp.m4` `LM_FIND_EMU_CC`).
- `coffix.c` → `coffix.exe` — small PE/COFF header fixer. MSVC's linker reads section *virtual* size
  (`.bss`) from the *raw* size field; gcc objects put it in the physical-address field. `coffix -e <file>`
  rewrites the section header in place (`SizeOfRawData ← PhysicalAddress`, `PhysicalAddress ← 0`) so
  mingw-gcc objects link with MSVC. `-v` = verbose; without `-e` it only reports.

## Constraints
- Bash under MSYS2 on Windows; MSVC tools (`cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`)
  must be on `PATH` (set up by `otp_build env_win32` / `SetupWSLcross.bat` in the WSL tree).
- Must keep the `cc.sh` (cc) / `ld.sh` (ld) / `ar.sh` (ar) / `emu_cc.sh` (emu_cc) / `rc.sh` / `mc.sh` names
  and calling conventions — `configure`, the static config caches and `erts/etc/common/Makefile.in` depend on them.
- Path arguments must be translated (`../msys2win_path.sh`); MSYS2 argument mangling is suppressed per-call
  with `MSYS2_ARG_CONV_EXCL`.
- `cc.sh` must satisfy `configure`'s compiler probes (it "makes configure happy", per its header) — be
  conservative when changing flag handling.
- Temp artifacts go to `/tmp` (`/tmp/tmpobj$$`, `/tmp/cl.exe.$$.*`, `/tmp/link.exe.$$.*`) and are cleaned up.

## Design Decisions
- **Two-compiler split**: MSVC (`cl.exe`) is the primary C compiler for OTP's C code, but the BEAM emulator
  is built with mingw gcc (`emu_cc.sh`) and then repaired with `coffix` — because gcc produces better code /
  is required for the JIT, while MSVC's linker is the final linker. `coffix` bridges the object-format difference.
- **Debug/log hooks**: each script honors an env var (`CC_SH_DEBUG_LOG`, `LD_SH_DEBUG_LOG`, `RC_SH_DEBUG_LOG`,
  `MC_SH_DEBUG_LOG`, `EMU_CC_SH_DEBUG_LOG`) to log the exact native command line it would run — the primary
  way to debug a failing build step.
- `rc.sh`/`mc.sh` discover their real tool at run time (scanning `PATH` and probing `-?` output) rather than
  baking in a path, because the correct Windows SDK `rc.exe`/`mc.exe` vary by installation.
- `MSYS2_ARG_CONV_EXCL` is exported per-invocation because MSYS2 rewrites `/`-prefixed flags
  (`-Fo…`, `-Fe…`) before the native tool sees them.

## Known Issues / Gotchas
- `coffix.exe` is generated into this directory at build time and is not tracked; do not expect it in a clean
  checkout. `emu_cc.sh` rebuilds it automatically.
- If Defender / Windows Search lock a freshly linked binary, the post-link `mt.exe` manifest step fails
  (the scripts literally print a hint about this in `ld.sh`).
- `cc.sh` hardcodes MSVC flags for several option classes (e.g. `-O*`→`-Ox -Zi`, `-g`→`-Z7`) — it is not a
  general-purpose cc, only enough for the Erlang build.

## Notes for Agents
- **Mirror changes into `../../cygwin_tools/vc/` and `../../wsl_tools/vc/`** (near-duplicates). The Cygwin
  tree additionally has a separate `mingw/` variant. Do not blindly copy — verify the host-specific
  path-conversion calls stay right.
- Interface consumers to re-check when changing flags: `erts/etc/common/Makefile.in` (win32,
  `USING_VC`→`-MT`/`-MD`), `make/autoconf/win32.config.cache.static`, `make/autoconf/otp.m4`.
- Cannot be tested on Linux; requires a real MSYS2 + MSVC Windows host.

## Dependencies
- MSVC: `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`.
- mingw-w64 gcc (`x86_64-w64-mingw32-gcc.exe`) for `emu_cc.sh`.
- MSYS2 bash + Windows `cmd.exe`; Perl (for `cc.sh` dependency generation).

## Test Strategy
- None automated. Exercised only by a full `otp_build env_win32` + `configure` + `boot`/`release` on Windows.

## See Also
- `../` → the host-level wrappers (`erl`, `erlc`, `javac.sh`, `msys2win_path.sh`, `w32_path.sh`, INI makers)
- `../../cygwin_tools/vc/`, `../../wsl_tools/vc/` → near-duplicate MSVC shims (read/compare; escalate writes to the parent)
- `../../../common/Makefile.in` → selects `rc.sh`/`mc.sh` and the `-MT`/`-MD` flags
