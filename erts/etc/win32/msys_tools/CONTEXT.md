# erts/etc/win32/msys_tools/ — MSYS2 toolchain shims

## Intent
Bash wrapper scripts that let an **MSYS2 (bash) environment on a Windows host** drive the
**Microsoft Visual C++ toolchain** (`cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`)
and `javac.exe` when building OTP for the native-Windows target (`TARGET=win32`).
They translate Unix-style command lines and `/`-paths into what the native Windows tools understand,
because neither autoconf nor the OTP makefiles know anything about MSVC.
MSYS2 provides no `cygpath`-like helper that is good enough here, so these scripts synthesize the
translation themselves with `cmd //C echo` + `sed`.

There are THREE near-duplicate shim trees under `erts/etc/win32/`:
- `../cygwin_tools/` — Cygwin host (uses `cygpath`, has a `mingw/` variant)
- `../wsl_tools/` — WSL host (uses `wslpath`, has `SetupWSLcross.bat`)
- this one — MSYS2 host (uses `cmd //C`; has `msys2win_path.sh`/`win2msys_path.sh`; only a `vc/` variant)
**A change here almost always must be mirrored into the other two trees**, and often also into
`../../common/Makefile.in` and `../../../otp_build` (the consumers). See "Notes for Agents".

## Who invokes these (contract)
- `otp_build` `echo_env_msys` (`./otp_build env_win32`) sets `CC=cc.sh`, `CXX=cc.sh`, `AR=ar.sh`,
  `RANLIB=true`, `OVERRIDE_TARGET=win32`, and prepends `<this dir>/vc:<this dir>` to `PATH`.
  `../cygwin_tools/` adds `mingw/` for the mingw flavor; this tree has no `mingw/`.
- `make/autoconf/win32.config.cache.static` hardcodes `CC=cc.sh`, `CXX=cc.sh`, `AR=ar.sh`,
  `CPP='cc.sh -E'`, `JAVAC=javac.sh`; `make/autoconf/otp.m4` `LM_FIND_EMU_CC` picks `emu_cc.sh`.
- `../../common/Makefile.in` (win32 branch) sets `RC=rc.sh`, `MC=mc.sh` and branches on
  `USING_VC=yes` (from `@MIXED_VC@`) to pick `-MT` vs `-MD` pdb flags.
- `Makefile.in` (top-level, `bootstrap_setup`, win32) runs `make_bootstrap_ini.sh`.
- `erts/Makefile` (`local_setup`, win32) runs `make_local_ini.sh`.
- External makefiles call **`w32_path.sh`** from `PATH` (`make/dep.mk`, `make/test_target_script.sh`,
  `lib/inets/src/*/Makefile`, `../nsis/Makefile`, `erts/emulator/utils/make_preload`, `erts/etc/win32/nsis/Makefile`).

## API Surface
- `erl`, `erlc` — thin exec wrappers around `erl.exe` / `erlc.exe`. Rewrite `/`-prefixed args (`-I/…`,
  `-o/…`, `-pa/…`, bare `/…`) through `msys2win_path.sh`, re-export `ERL_TOP` in Windows form, then `eval`.
  `erlc` also supports `--echo_only` (used internally to translate `+'{preproc_flags,…}'`). The header
  comment says "cygwin" but these run under MSYS2.
- `javac.sh` — wrapper for `javac.exe`; converts `:`-separated `CLASSPATH` to `;`-separated with Windows
  paths, and translates `-d/-I/-o` and absolute file args.
- `msys2win_path.sh` — **MSYS→Windows** conversion. `-m` = mixed (forward slashes kept), `-a` = absolute
  (resolves `..` via a real Windows `for` loop). Uses `cmd //c echo` /`cmd //C echo` (never `cygpath`).
- `win2msys_path.sh` — **Windows→MSYS** conversion. Drive letter `C:\\`→`/c/`, `\`→`/`; strips spaces by
  asking Windows for DOS 8.3 short names (`cmd //C "for %i in (".") do @echo %~fsi"`), writing a temp `.bat`
  when the path itself contains spaces.
- `w32_path.sh` — public generic converter (`-m|-w|-d|-u [-a]`), implemented on top of the two above.
  It is the interface consumed by external Makefiles; the other scripts in THIS tree call
  `msys2win_path.sh`/`win2msys_path.sh` directly.
- `reg_query.sh` — read a `REG_*` value from the Windows registry (`<key> <valuename>`); writes a temp
  `.bat`, uses the 64-bit `reg.exe` via the `sysnative` alias when running under WOW64, then strips the
  `… REG_xxx ` prefix from the output.
- `make_local_ini.sh` — writes `$ERL_TOP/bin/erl.ini` (`[erlang]` `Bindir`/`Rootdir`, Progname=erl) that
  `bin/erl.exe` reads to locate `erlexec.dll` / `beam.dll`. `Bindir` is `…\bin\win32`; path backslashes are
  doubled so the INI text is correct (see `../erl.c`, which also escapes `%` as `%%`).
- `make_bootstrap_ini.sh` — same INI for the bootstrap tree; takes explicit `<rootdir> <bindir>` and is run
  by the top-level `Makefile.in` `bootstrap_setup` target.
- `vc/` — the MSVC shims (`cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c`).
  See `./vc/CONTEXT.md`.

## Constraints
- These are **bash scripts run under MSYS2 on a Windows host**; they must not assume a POSIX-only world
  (they shell out to `cmd`, and rely on `cl.exe`/`link.exe` being on `PATH`).
- MSYS2 mangles `/`-prefixed arguments, so the scripts that invoke MSVC export `MSYS2_ARG_CONV_EXCL`
  (`cc.sh` → `-FoC`, `rc.sh` → `-Fo`, `mc.sh` → empty, `emu_cc.sh` → `-FeC`) before the tool call.
- Never pass a bare Windows path to `cl`/`link`/`lib`/`rc`/`mc`/`javac`; always go through the path scripts.
- INI files: values must survive backslash + `%`-escaping rules implemented in `../erl.c`.
- Scripts are invoked by name from `PATH` (`cc.sh`, `ld.sh`, …), so names/locations are part of the contract.
- Only `w32_path.sh` is tracked executable (mode 100755); the others are 100644 and are run via
  `sh script` / `PATH` lookup — do not rely on the exec bit.

## Design Decisions
- **Shims instead of native configure/MSVC support**: OTP's `configure` and makefiles are Unix-centric
  (`cc`, `ar`, `-L`, `-l`, `-o`). Rather than teach them MSVC, `otp_build env_win32` sets `CC=cc.sh` etc.
  and a static config cache, so configure "builds with a cc" that happens to be a script wrapping `cl.exe`.
  `cc.sh`'s own header admits it is "made to work for Erlang builds and to make configure happy".
- **Three near-identical copies rather than one env-detecting script**: the path-conversion primitive
  differs per host (Cygwin `cygpath`; WSL `wslpath`/`SetupWSLcross.bat`;
  MSYS2 `cmd //C`), and each environment ships a different `PATH` layout / compiler setup. The copies have
  already drifted (this tree is the only one with `msys2win_path.sh`/`win2msys_path.sh`; Cygwin is the only
  one with `mingw/`; WSL is the only one with `SetupWSLcross.bat`). This duplication is
  the observed mechanism, not a single abstraction with runtime detection.
- **MSYS2 path synthesis via `cmd //c echo`**: unlike Cygwin there is no reliable `cygpath`, so
  `msys2win_path.sh`/`win2msys_path.sh` build the conversion from `cmd` output and `sed`, including the
  DOS-8.3 short-name trick to cope with spaces in paths.
- `make_local_ini.sh` vs `make_bootstrap_ini.sh`: kept separate because the bootstrap stage must point the
  shipped `erl.exe` at distinct root/bindir locations before `bin/$(TARGET)` exists.

## Known Issues / Gotchas
- `w32_path.sh` here is NOT called by the other scripts in this tree (they call
  `msys2win_path.sh`/`win2msys_path.sh` directly) — but it MUST stay, because external Makefiles on `PATH`
  still invoke `w32_path.sh` (see "Who invokes these"). Removing it breaks those builds.
- `msys2win_path.sh` with `-a` uses `cmd //C echo` on the *current* directory after `cd`, so relative
  paths with a trailing `..` are handled by a special case in the script.
- Several scripts write temp files to `/tmp` (`.bat` files, `/tmp/cl.exe.$$.*`, `/tmp/tmpobj$$`); they rely
  on MSYS2 `/tmp` existing and on `cmd //C <path>` being able to see those paths.
- Comments in `erl`/`erlc`/`javac.sh` still reference "cygwin"/"cygpath" — historical wording, ignore it.

## Notes for Agents
- **Any edit here must be mirrored into `../cygwin_tools/` and `../wsl_tools/`** (near-duplicates), and you
  should re-check `../../common/Makefile.in` (USING_VC/MIXED_VC, `RC`/`MC`, `-MT`/`-MD`) and `../../../otp_build`
  (`echo_env_msys`) — those define the interface the scripts must satisfy.
- The three trees are compared with `diff -rq erts/etc/win32/{cygwin_tools,wsl_tools} erts/etc/win32/msys_tools`.
- Do NOT introduce `cygpath` here — that is the Cygwin tree's mechanism.
- Changes cannot be validated on Linux CI; a real MSYS2 + MSVC Windows host is required.

## Dependencies
- MSYS2 bash on Windows; Windows `cmd.exe`.
- MSVC toolchain on `PATH`: `cl.exe`, `link.exe`, `lib.exe`, `rc.exe`, `mc.exe`, `mt.exe`, `reg.exe`
  (64-bit via `sysnative`).
- A JDK (`javac.exe`) for `javac.sh`; mingw-w64 gcc for `vc/emu_cc.sh`.

## Test Strategy
- None automated. These scripts are exercised only by an actual `./otp_build env_win32` + `configure` +
  `boot`/`release` cycle on a Windows host (MSYS2). There is no unit/suite test for them.

## See Also
- `./vc/` → the MSVC shims (`cc.sh`, `ld.sh`, `ar.sh`, `rc.sh`, `mc.sh`, `emu_cc.sh`, `coffix.c`)
- `../cygwin_tools/`, `../wsl_tools/` → sibling shim trees (near-duplicates; keep in sync — read/compare, escalate writes to the parent)
- `../../common/Makefile.in` → consumes `cc.sh`/`ld.sh`/`ar.sh`/`rc.sh`/`mc.sh` and `USING_VC`/`MIXED_VC`
- `../erl.c` → reads the `erl.ini` written by `make_local_ini.sh`/`make_bootstrap_ini.sh`
- `../../../otp_build` → `echo_env_msys` sets the environment that puts these on `PATH`
- `../../../HOWTO/INSTALL-WIN32.md` → user-facing Windows build instructions

## Routing Table
- `./vc/` → MSVC compiler/linker/archiver/resource/message shims and `coffix.c` (see `./vc/CONTEXT.md`)
