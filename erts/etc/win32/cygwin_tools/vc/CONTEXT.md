# erts/etc/win32/cygwin_tools/vc/ — Visual C++ shims

## Intent
The MSVC half of the Cygwin toolchain shims: scripts that present a GCC-ish command line and run the Microsoft tools instead (`cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`).
`otp_build env_win32` prepends this directory to `PATH` (before `../`), so `cc.sh`/`ld.sh`/`ar.sh`/`rc.sh`/`mc.sh`/`emu_cc.sh` are what the build finds for `CC`, `AR`, `RC`, `MC` and `EMU_CC`.
Configure-time pins: `make/autoconf/win*.config.cache.static` (`ac_cv_prog_CC=cc.sh`, `ac_cv_prog_AR=ar.sh`, a commented `ac_cv_prog_emu_cc=.../vc/emu_cc.sh`); `erts/configure.ac` sets `LD='$(CC)'`.

## API Surface
- `cc.sh` — compiler *and* linker driver. Parses the GCC-ish command line and maps it to MSVC:
  `-c` → compile only, `-o/-o*` → `-Fo` output, `-I/*` → `-I<mixed path>`, `-D*`/`-EH*`/`-TP` pass-through, `-l*`/`*.o` → deferred to `ld.sh`, `*.c/.cc/.cpp` → sources.
  `-g|-ggdb` → `-Z7` + (unless forced) `-MDd` + `-g` for the link; `-O*` → `-Ox -Zi` (the `-Owx` variant → `-Ob2ity -Gs -Zi`), `-O0`/`-Og` ignored; `-mt/-MT`, `-md/-MD`, `-ml/-ML`, `-mdd/-MDD/-MDd`, `-mtd/-MTD/-MTd`, `-mld/-MLD/-MLd` set the stdlib and force it (default `-MD`).
  `-E` → preprocess; `-MM` → preprocess and emit `#line`-derived make dependencies. Always adds `-nologo -D__WIN32__ -DWIN32 -DWINDOWS -D_WIN32 -DNT -D_CRT_SECURE_NO_DEPRECATE`.
  When linking, it compiles every source into `/tmp/tmpobj$$`, then invokes `ld.sh` with those objects plus the stdlib implied by `-M*` (`-MT`→`-lLIBCMT`, `-MD`→`-lMSVCRT`, `-ML`→`-lLIBC`, `…d` variants).
  `CC_SH_DEBUG_LOG` appends both the incoming and the generated command lines (used for debugging Windows builds).
- `ld.sh` — linker driver for `link.exe`. `-L/*`→`-libpath:`, `-l<name>`→`"<name>.lib"` (special cases `-lMSVCRT*`, `-lLIBCMT*`, `-lsocket`→`WS2_32.LIB IPHLPAPI.LIB`), `-implib:`/`-def:`/`-g`, `-dll`, `-o*`; always appends `-nologo -incremental:no -largeaddressaware`, a stdlib (`MSVCRT.LIB`/`MSVCRTD.LIB`/`LIBCMT.LIB`/…) and default libraries (`kernel32/advapi32/gdi32/user32/comctl32/comdlg32/shell32`). Generates `.pdb` next to exe/dll, then `mt.exe`-embeds a manifest in `$OUTPUTRES` after rewriting its `<trustInfo>` to `requestedExecutionLevel level="AsInvoker"` (disables installer virtualization); `chmod +x` on the produced file works around a Cygwin/Samba glitch that would otherwise make `configure` think the compiler failed.
- `ar.sh` — `lib.exe` driver: `-out:<path>` (converted) plus all other args, spaces escaped.
- `rc.sh` — `rc.exe` driver: locates `rc.exe` by scanning `PATH` for one whose `-?` output says "resource compiler" (avoids non-MSDK `rc.exe`), converts `-I`/input paths, writes with `-Fo`.
- `mc.sh` — `mc.exe` driver: locates `mc.exe` the same way, converts `-I`, `cd`s into the `-o` output directory before running (message compiler writes beside the source otherwise).
- `emu_cc.sh` — the `EMU_CC` compiler (see `make/autoconf/otp.m4`, `LM_FIND_EMU_CC`): compiles with mingw `gcc` (`x86_64-w64-mingw32-gcc.exe` when `CONFIG_SUBTYPE=win64`, else `gcc`) plus `-D__WIN32__ -DWIN32 -DWINDOWS -fomit-frame-pointer`, then runs `coffix.exe -e` on the object. Needed because `cl.exe` cannot compile the computed gotos in `beam_emu.c`. `-MM` mode skips coffix. It self-bootstraps: rebuilds `coffix.exe` with `cl.exe` when `coffix.c` is newer.
- `coffix.c` → built on demand as `coffix.exe`; patches PE/COFF section headers where gcc/mingw put the virtual size in `Misc.PhysicalAddress` with `SizeOfRawData == 0` (typical `.bss`), moving it to `SizeOfRawData` so the MS linker sizes the section correctly. Byte-identical to `../../mingw/coffix.c` and `../../../msys_tools/vc/coffix.c`/wsl copy.

## Constraints
- Same environment rules as `../` (Cygwin bash, `cygpath` conversion before calling MS tools; `/tmp` temp files).
- `ld.sh` expects to be called by `cc.sh` (it relies on `cc.sh` having translated sources to objects) but is also usable standalone with `.o`/`-l*` args.
- Never reorder `-nologo`/stdlib/default-library handling without checking `../../common/Makefile.in` (`USING_VC=yes` selects `ml`/`mc` behaviour and `-MT`).
- `emu_cc.sh`/`coffix.c` must keep working for both `CONFIG_SUBTYPE=win64` and 32-bit builds (the gcc names differ).

## Known Issues / Gotchas
- `mc.sh`/`rc.sh` scan `PATH` and probe `-?`; on a machine with several `rc.exe`/`mc.exe` (e.g. Wine, Cygwin packages, Windows SDK) the wrong one may match.
- `ld.sh` prints `tail -n +2` of linker stderr to drop the Microsoft banner; `cc.sh` does the same for stdout/stderr, so extra banner lines leak into `configure` output.
- `cc.sh` rejects multiple sources with a single `-o *.o`.
- Debug flags: `-g` turns on `/MDd` (debug CRT) unless the stdlib was forced with `-m*`; mixing object files built with different `-M*` settings produces link errors in `link.sh`'s stdlib choice.
- The `-Owx` (wxErlang) optimization case is hardcoded; it silently overrides `-g`.

## Dependencies
- MSVC: `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`; Cygwin `cygpath`.
- mingw gcc for `emu_cc.sh` (and `cl.exe` for building `coffix.exe`).
- Environment: `CONFIG_SUBTYPE` (`win64`/`arm64`/`x64_arm64`), `CC_SH_DEBUG_LOG`/`LD_SH_DEBUG_LOG`/`MC_SH_DEBUG_LOG`/`RC_SH_DEBUG_LOG`/`EMU_CC_SH_DEBUG_LOG` (optional debug logs).

## Test Strategy
- No automated tests. Exercised only by a real Windows build (`otp_build configure`/`boot`), where `cc.sh` runs for every configure test. Use `TYPE=debug` to bypass `EMU_CC`/coffix when isolating problems.

## Routing Table
- `../` → Cygwin shim front-ends (`erl`, `erlc`, `javac.sh`, `w32_path.sh`, `make_*_ini.sh`, `reg_query.sh`)
- `../mingw/` → mingw/gcc variants of `cc.sh`/`ld.sh`/`ar.sh`/`rc.sh`/`mc.sh`/`emu_cc.sh`
- `../../../common/Makefile.in` → consumer (`RC=rc.sh`, `MC=mc.sh`, `USING_VC`)
