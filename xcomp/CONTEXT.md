# xcomp — Cross-compilation configuration files

## Intent
Flat directory of shell-script fragments describing *how* to cross compile Erlang/OTP for a given target.
Each fragment is dot-sourced by `otp_build` and translated into `configure` arguments, so its variables become build-tool/feature overrides.
Nothing here is compiled or installed; the files are developer/CI-facing configuration samples with no build product of their own.

## API Surface
- `erl-xcomp.conf.template` — master template. Contains every supported variable, commented out, with per-variable semantics, defaults and warnings. It doubles as user documentation. Copy it, uncomment what you need, and pass the copy to `otp_build configure --xcomp-conf=<FILE>`. `--xcomp-conf` is an `otp_build`-only argument: plain `configure` rejects it, so a direct `./configure` run must instead be given each variable as a `VAR=VALUE` argument or environment variable.
- `erl-xcomp-vars.sh` — defines the single shell variable `erl_xcomp_vars` (19 names). `otp_build` sources this to know which variables to translate into `--<var>=<value>` `configure` arguments.
- `README.md` — 5 lines; only points to `HOWTO/INSTALL-CROSS.md`.
- `erl-xcomp-<target>.conf` (17 files) — ready-made configs, each the template with a few lines uncommented. Naming is loose: some are `<cpu>-<os>` (e.g. `erl-xcomp-mips-linux.conf`), some add vendor/abi (e.g. `erl-xcomp-armv8-rpi3-linux-gnueabihf.conf`), some are tool-version-named (`erl-xcomp-TileraMDE2.0-tilepro.conf`), one is dynamic (`erl-xcomp-host-iossimulator.conf`, uses `` `arch` ``).

## How cross compilation is driven
1. `otp_build configure --xcomp-conf=<FILE>` sources the conf file inside `try_cross_configure ()` (tilde expansion is done here, since `configure` cannot do it). Cross configure is triggered either by `--xcomp-conf` or by `--build`/`--host` canonicalizing to different systems.
2. `erl_xcomp_build` / `erl_xcomp_host` become `--build=` / `--host=`; `erl_xcomp_configure_flags` is appended to `CONFIG_FLAGS`; the `erl_build_tool_vars` (`erl-build-tool-vars.sh`) and remaining `erl_xcomp_vars` are turned into `--VAR=value` configure args by `env_to_config_flags`.
3. Autoconf then treats those as *precious variables*, so they override the results of run-time probe tests that cannot be executed on the target (which is why the file is described as overriding/backing configure's guesses).
4. Only variables in the two `*-vars.sh` lists, plus `CC/CFLAGS/...` build-tool vars, are guaranteed visible to every sub-`configure`. Arbitrary variables need to be exported in the environment instead.
5. `erl_xcomp_sysroot` / `erl_xcomp_isysroot` are special: without a sysroot, `crypto`, `odbc`, `ssh` and `ssl` are skipped (see `make/autoconf/otp.m4:ERL_XCOMP_SYSROOT_INIT`).

### Variable groups (see the template for authoritative semantics)
- `otp_build`-only: `erl_xcomp_build`, `erl_xcomp_host`, `erl_xcomp_configure_flags` (no effect when calling `configure` directly).
- Build tools: `CC CFLAGS STATIC_CFLAGS CFLAG_RUNTIME_LIBRARY_PATH CPP CPPFLAGS CXX CXXFLAGS LD LDFLAGS LIBS RANLIB AR GETCONF`, plus the `DED_LD*` (dynamically-loaded driver linker — set all or none) and `LFS_*` (large-file support — set all or none) trios. Usable for native builds too.
- Sysroots: `erl_xcomp_sysroot`, `erl_xcomp_isysroot` (defaults to `sysroot`).
- Feature/bug overrides (`yes|no`, most documented as "defaults to X"): `erl_xcomp_bigendian`, `erl_xcomp_double_middle_endian`, `erl_xcomp_linux_nptl`, `erl_xcomp_linux_usable_sigusrx`, `erl_xcomp_linux_usable_sigaltstack`, `erl_xcomp_linux_clock_gettime_correction`, `erl_xcomp_poll`, `erl_xcomp_kqueue`, `erl_xcomp_putenv_copy`, `erl_xcomp_reliable_fpe`, `erl_xcomp_getaddrinfo`, `erl_xcomp_gethrvtime_procfs_ioctl`, `erl_xcomp_clock_gettime_cpu_time`, `erl_xcomp_after_morecore_hook`, `erl_xcomp_dlsym_brk_wrappers`, `erl_xcomp_posix_memalign`, `erl_xcomp_code_model_small`. Set these only if you really know the target; configure warns whenever a default is used.

### Available target configs
| File | `erl_xcomp_host` | Env vars you must set first |
| --- | --- | --- |
| `erl-xcomp-aarch64-darwin.conf` | `aarch64-apple-darwin` | none — uses `xcrun -sdk macosx` |
| `erl-xcomp-arm64-android.conf` | `aarch64-linux-android` | `NDK_ABI_PLAT` (e.g. `android21`), NDK `clang` on `PATH` |
| `erl-xcomp-arm-android.conf` | `arm-linux-androideabi` (armv7a, softfp) | `NDK_ABI_PLAT` (e.g. `androideabi16`) |
| `erl-xcomp-x86_64-android.conf` | `x86_64-linux-android` | `NDK_ABI_PLAT` |
| `erl-xcomp-arm-ios.conf` | `arm-apple-ios` (armv7 device) | none — `xcrun -sdk iphoneos` |
| `erl-xcomp-arm64-ios.conf` | `arm64-apple-ios` (device) | none — `xcrun -sdk iphoneos` |
| `erl-xcomp-arm64-iossimulator.conf` | `arm64-apple-iossimulator` | none — `xcrun -sdk iphonesimulator` |
| `erl-xcomp-x86_64-iossimulator.conf` | `x86_64-apple-iossimulator` | none |
| `erl-xcomp-host-iossimulator.conf` | `` `arch` ``-apple-iossimulator | none — picks the host arch at runtime |
| `erl-xcomp-arm-linux.conf` | `arm-wrs-linux-gnueabi` | `ARM_SYSROOT` |
| `erl-xcomp-armv8-rpi3-linux-gnueabihf.conf` | `armv8-rpi3-linux-gnueabihf` | `RPI_SYSROOT` |
| `erl-xcomp-avr32-atmel-linux-gnu.conf` | `avr32-atmel-linux-gnu` | `ATMEL_SYSROOT_AVR32` |
| `erl-xcomp-mips-linux.conf` | `mips-linux` | none (minimal: only `CFLAGS=-Os`) |
| `erl-xcomp-powerpc64-bgq-linux.conf` | `powerpc64-bgq-linux` | none — hardcodes Blue Gene `/bgsys/drivers/ppcfloor` |
| `erl-xcomp-powerpc-dso-linux-gnu.conf` | `powerpc-wrs-linux-gnu` | `DSO_SYSROOT_POWERPC` |
| `erl-xcomp-TileraMDE2.0-tilepro.conf` | `tile` | `TILERA_ROOT` |
| `erl-xcomp-x86_64-saf-linux-gnu.conf` | `x86_64-saf-linux-gnu` | `SAF_SYSROOT_X86_64` |

Recurring `erl_xcomp_configure_flags` patterns: iOS targets use `--without-{termcap,wx,javac,odbc,debugger,observer,cdv,et} --enable-{builtin-zlib,static-nifs,static-drivers}` and often `--disable-jit`; Android targets use `--without-{termcap,wx} --enable-{builtin-zlib,deterministic-build}` and `LDFLAGS="-static-libstdc++"`; 32-bit Linux targets add `--disable-year2038`.

## Constraints
- The files are POSIX shell fragments sourced by `otp_build`, **not** a config format: backticks, `$VAR` expansion and `export` are all active.
- When a direct `./configure` run is given these variables via the environment, unset them again before `make`, or they leak into application make variables.
- Keep `erl-xcomp-vars.sh` in sync with the template and with the `AC_ARG_VAR` declarations in `make/autoconf/otp.m4` (this directory's own NOTE comments still name the pre-move path `erts/aclocal.m4`).
- Every per-target `.conf` is a copy of the template body with a handful of lines uncommented, so a template change normally implies touching all 17 target files.
- Do not add variables that are not in `erl_xcomp_vars`/`erl_build_tool_vars` and expect them to reach sub-configures.

## Known Issues
- The NOTE in `erl-xcomp-vars.sh` (and in root `erl-build-tool-vars.sh`), and the patch checklist in `HOWTO/INSTALL-CROSS.md`, say to also update `$ERL_TOP/erts/aclocal.m4`; that file no longer exists — the autoconf helpers live in `make/autoconf/otp.m4`, which holds the `AC_ARG_VAR` declarations for 16 of the 19 vars. The checklist's recommendation is otherwise authoritative: a new variable touches `erl-xcomp.conf.template`, `erl-xcomp-vars.sh`, `erl-build-tool-vars.sh`, `xcomp/README.md` (if it grows a list), `HOWTO/INSTALL-CROSS.md` and the `erl-xcomp-*.conf` samples.
- `erl_xcomp_code_model_small` and `erl_xcomp_linux_clock_gettime_correction` are listed in `erl-xcomp-vars.sh` and documented in the template/HOWTO but are currently read by **no** configure script — setting them has no effect.
- `erl_xcomp_posix_memalign` is listed but is not an `AC_ARG_VAR`; it is read directly in `erts/configure.ac` (search `AS_CASE([X$erl_xcomp_posix_memalign]`).
- The template spells the middle-endian variable `erl_xcomp_double_middle_endian` while its prose (and `HOWTO/INSTALL-CROSS.md`) calls it `erl_xcomp_double_middle`; the `_endian` spelling is the real one. The template's line for it also lacks a trailing `=`, unlike its siblings.
- Many configs reference site-specific absolute paths (`/ldisk/cross/gcc-toolchain/sysroot`, `/bgsys/drivers/ppcfloor/gnu-linux/bin`, `/sysroot/path/handled/by/the/Android/NDK`) — they are examples, not portable defaults.

## Notes for Agents
- Do not delegate into subdirectories: this node is flat (no subdirectories, no tests, no build artifacts).
- `HOWTO/INSTALL-CROSS.md` duplicates the template's variable list in prose; it is the source for the built installation guide (`system/doc/docs.exs` maps it to `system/doc/installation_guide/INSTALL-CROSS.md`) and carries a `%OTP-REL%` placeholder, so a variable added to the template must land there too.
- Consumers of a conf file, besides `otp_build`/`configure`: `lib/common_test/test_server/ts_install.erl` (`parse_xcomp_file/1`) parses it for `erl_xcomp_host`, `erl_xcomp_build` and the environment exports to set up cross test runs; `.github/scripts/build-macos.sh` is driven with `--xcomp-conf=./xcomp/erl-xcomp-arm64-ios.conf` from `.github/workflows/reusable-build-ios.yaml`.
- `erl-xcomp-aarch64-darwin.conf` and the ios/iossimulator configs require macOS + Xcode command-line tools (`xcrun`); they cannot be exercised on Linux.
- Apple/Android confs `export` helper variables (`XCOMP_SDK`, `XCOMP_SDKROOT`, `XCOMP_ARCH`) that only exist inside the sourced file.

## Dependencies
- `otp_build`, the per-directory `configure` scripts (generated from `configure.src` / `*.ac`), and `make/autoconf/config.{guess,sub}`.
- Target toolchains: Xcode/`xcrun` (Darwin/iOS), Android NDK (Android), plus the various `<host>-gcc` cross toolchains and sysroots named above.
- Documentation counterpart: `HOWTO/INSTALL-CROSS.md`, `HOWTO/INSTALL-ANDROID.md`, `HOWTO/INSTALL-IOS.md`, `HOWTO/INSTALL-RASPBERRYPI3.md`.

## Test Strategy
No automated tests live here. Validation is manual/CI: the configs are exercised by the iOS build workflow and by developers running `otp_build configure --xcomp-conf=...` followed by `make`. A typo in a variable name silently does nothing (autoconf only warns about unrecognized `--x` options), so verify with a real configure run and grep the produced `config.log`/`Makefile`.

## See Also
- `../otp_build` — sources `erl-xcomp-vars.sh` and the given `--xcomp-conf` file (`try_cross_configure`, `env_to_config_flags`).
- `../erl-build-tool-vars.sh` — the build-tool half of the variable namespace.
- `../make/autoconf/otp.m4` — `AC_ARG_VAR`/`ERL_XCOMP_SYSROOT_INIT` declarations; where the schema is actually validated.
- `../HOWTO/INSTALL-CROSS.md` — full prose documentation of the mechanism and every variable.
- `../.github/dockerfiles/Dockerfile.cross-compile` — example of doing the same thing via exported env vars without a conf file.

## Status
Complete for the current HEAD (OTP 30.0-rc0); flat directory, no child nodes.
