# erts/etc/win32/cygwin_tools/mingw/ — mingw/gcc shims (Cygwin)

## Intent
mingw/gcc variants of the Cygwin toolchain shims, used when OTP is built for `TARGET=win32` with mingw instead of Visual C++.
They are *not* on the default `PATH`: `otp_build env_mingw32` (the "Cygwin build without microsoft visual C++ (dead?)" branch in `otp_build`) prepends `erts/etc/win32/cygwin_tools/mingw:erts/etc/win32/cygwin_tools` to `PATH` for a separate shell, and exports `MINGW_EXE_PATH` (mingw bin directory) and `WINE_EXE_PATH` (wine's `wmc.exe`/`wrc.exe`).
Because this directory comes before `../`, its `cc.sh`/`ld.sh`/`ar.sh`/`rc.sh`/`mc.sh` shadow the MSVC ones in that environment.

## API Surface
- `cc.sh` — gcc driver: `COMMON_CFLAGS="-mwindows -D__WIN32__ -DWIN32 -DWINDOWS -D_WIN32 -DNT -DWIN32_MINGW"`; requires `MINGW_EXE_PATH`; `-MM|-M` → dependency generation with `LINKING=false`; sources are collected into `LINKSOURCES` and compiled/linked with minGW gcc.
- `ld.sh` — mingw linker driver: `-shared`/`-dll` for DLLs, `-Xlinker --out-implib`, `-Xlinker --entry`, `-l<name>` → `-l"<path>"`, default libraries `-lkernel32 -ladvapi32 -lgdi32 -luser32 -lcomctl32 -lcomdlg32 -lshell32`, `-lws2_32` for `-lsocket`; runs `$MINGW_EXE_PATH/gcc`; requires `MINGW_EXE_PATH`. `-def:` handling is deliberately disabled (commented out — it made `ld` core dump).
- `ar.sh` — `$MINGW_EXE_PATH/ar.exe` with `rcv "<archive>"`, accepting both `-o` and `-out:`; requires `MINGW_EXE_PATH`.
- `rc.sh` — `$MINGW_EXE_PATH/windres.exe`, output via `-o`; requires `MINGW_EXE_PATH`.
- `mc.sh` — message compiler via wine: `$WINE_EXE_PATH/wmc.exe -i` to produce a `.rc`, then `$WINE_EXE_PATH/wrc.exe -i` to compile it to `.res`; the file-name mapping (`foo.mc` → `foo.rc`) and both output prefixes (`wmc`/`wrc`) are handled here. Requires `WINE_EXE_PATH`.
- `emu_cc.sh` — the `EMU_CC` compiler for this environment: plain `gcc` with `-D__WIN32__ -DWIN32 -DWINDOWS -fomit-frame-pointer`, then `coffix.exe -e` on the object (built on demand from `coffix.c` with `cl.exe`). `TOOLDIR` points at this directory.
- `coffix.c` — identical to `../vc/coffix.c`: patches COFF section headers whose virtual size sits in `Misc.PhysicalAddress` with `SizeOfRawData == 0`, so MSVC's linker can size `.bss` sections from gcc/mingw objects.

## Constraints
- All scripts abort with a message when `MINGW_EXE_PATH` (`ar.sh`, `cc.sh`, `ld.sh`, `rc.sh`) or `WINE_EXE_PATH` (`mc.sh`) is unset — that environment setup only happens via `otp_build env_mingw32`/`env_win32`.
- `-def:` is intentionally not passed to the linker here; do not "fix" it without reproducing the original `ld` crash.
- Same Cygwin/bash/path-conversion rules as `../`; temp files live in `/tmp`.

## Known Issues / Gotchas
- This tree only ships `cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c` — there are no `erl`/`erlc`/`javac.sh`/`w32_path.sh` here; those come from `../` (both directories are on `PATH` together).
- Needs a mingw gcc plus wine's message/resource compilers; the mingw build path is described as "dead?" in `otp_build` and is the least exercised of the three environments.
- `coffix.c` is duplicated with `../vc/`; keep the two copies in sync (and with `../../msys_tools/vc/coffix.c`, `../../wsl_tools/vc/coffix.c`).

## Dependencies
- mingw gcc/ar/windres (`MINGW_EXE_PATH`), wine `wmc.exe`/`wrc.exe` (`WINE_EXE_PATH`), `cl.exe` (only to build `coffix.exe`), Cygwin `cygpath`.

## Test Strategy
- No automated tests. Only exercised by a full `otp_build env_mingw32` + configure/boot build on a Cygwin+mingw+wine host.

## Routing Table
- `../` → Cygwin front-end shims (`erl`, `erlc`, `javac.sh`, `w32_path.sh`, `make_*_ini.sh`, `reg_query.sh`)
- `../vc/` → the MSVC shims that this tree shadows when `env_mingw32` is used
