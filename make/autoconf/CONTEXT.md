# make/autoconf — shared autoconf aux dir + OTP macro library

## Intent
Shared aux directory for **every** `configure` script in the tree, plus the OTP-local m4 macro library.
Each `configure.ac` does `m4_include([otp.m4])` after `AC_INIT` and points `AC_CONFIG_AUX_DIR(S)` at `${ERL_TOP}/make/autoconf`, so this directory defines the common configure vocabulary (tool detection, platform probes, cross-compilation, DED flags) for `make/`, `erts/` and all `lib/*` apps.

## API Surface
Files:
- `otp.m4` (~3.5k lines) — hand-written macro library; the real API of this directory.
- `config.guess`, `config.sub`, `install-sh` — GNU Autoconf 2.72 scripts, imported verbatim (FFS, `MIT AND (GPL-3.0-or-later WITH Autoconf-exception-generic-3.0)`); **master copies** — `otp_build` copies them into `erts/autoconf/` and `lib/common_test/test_server/`.
- `vendor.info` — SPDX/vendor metadata for those three files (see Notes).
- `win{32,64,64-arm64,64-x64_arm64}.config.cache.static` — precomputed autoconf caches for Windows cross builds (see Notes).

### otp.m4 macro inventory (grouped by purpose)
Naming: `LM_*` = generic/autoconf-ish helper, `ERL_*` = Erlang-specific, `ETHR_*` = ethread internals (prefix not applied consistently — see header comment).

Logging / diagnostics
- `LM_LOG(msg)` — printf to `config.log` (`AS_MESSAGE_LOG_FD`).

Warnings
- `ERL_PUSH_WERROR` / `ERL_POP_WERROR` — strip `-Werror` from `CFLAGS` during configure, stash it in `WERRORFLAGS`, re-add it for the erts build.

Canonical system type / cross-compilation
- `ERL_CANONICAL_SYSTEM_TYPE` — `AC_CANONICAL_{HOST,BUILD,TARGET}` plus the legacy `local-*-windows` → `win32` rewrite; errors out when cross-compiling with identical canonical build/host.
- `ERL_XCOMP_SYSROOT_INIT` — normalizes `erl_xcomp_sysroot` / `erl_xcomp_isysroot`, sets `erl_xcomp_without_sysroot`.
- `LM_CHECK_GETCONF` — cross-aware `getconf` lookup (`<host>-getconf`, sysroot paths); sets `GETCONF`.
- `LM_HARDWARE_ARCH` — maps `host_cpu`/`uname -m` to `ARCH` (adjusted by `ac_cv_sizeof_void_p`); `AC_SUBST(ARCH)`.
- `LM_PRECIOUS_VARS` — declares all `AC_ARG_VAR`s (CC/CFLAGS/LD/DED_LD*/RANLIB/AR/GETCONF/EX_DOC, `erl_xcomp_*`). Must be expanded early (before other macros).

Tool / flag detection
- `LM_PROG_LD` — finds a linker, then forces `LD=${CC}` (Windows: `ld.sh`).
- `LM_FIND_EMU_CC` — finds a compiler handling computed goto; `AC_SUBST(EMU_CC)`, defines `NO_JUMP_TABLE` on failure.
- `LM_PROG_INSTALL_DIR` — "how to make dirs with parents"; `AC_SUBST(INSTALL_DIR)`; usable by any OTP app.
- `LM_PROG_PERL5` — `PERL` (absolute path or `false`).
- `LM_TRY_ENABLE_CFLAG(flag, var [,prologue])`, `LM_CHECK_ENABLE_CFLAG(flag, boolvar)`, `LM_CHECK_RUN_CFLAG(flag, boolvar)` (runs the binary — needed for PGO), `LM_TRY_ENABLE_LDFLAG(flag, var)` — probe a flag with `-Werror` and append it to the caller's variable on success.
- `ERL_TRY_LINK_JAVA(CLASSES, BODY [,found [,not-found]])` — `$JAVAC` link test (common_test test_server).

Windows / mixed environments
- `LM_WINDOWS_ENVIRONMENT` — detects MSYS/Cygwin/WSL mixed with VC or MinGW; sets and `AC_SUBST`s `MIXED_VC`, `MIXED_MINGW` (plus shell-only `MIXED_CYGWIN/MSYS/WSL`), appends `-O2` and `-DERTS_MIXED_VC`/`-DERTS_MIXED_MINGW` to `CPPFLAGS`.

Platform feature probes (portability)
- `LM_DECL_SO_BSDCOMPAT`, `LM_DECL_INADDR_LOOPBACK`, `LM_STRUCT_SOCKADDR_SA_LEN`, `LM_SYS_IPV6`, `LM_SYS_MULTICAST`, `LM_DECL_SYS_ERRLIST`, `LM_CHECK_FUNC_DECL(func, decl [,includes [,found [,not-found]]])`.
- `AC_C_DOUBLE_MIDDLE_ENDIAN` — non-`LM_`/`ERL_` prefixed extension of the autoconf namespace; sets `ac_cv_c_double_middle_endian` and defines `DOUBLE_MIDDLE_ENDIAN`.

Threads / ethread
- `LM_CHECK_THR_LIB` — sets `THR_DEFS`, `THR_LIBS`, `THR_LIB_NAME`, `THR_LIB_TYPE` (win32_threads / pthread / NPTL detection); documented as usable by any OTP app.
- `ERL_INTERNAL_LIBS` — probes `-lkstat`; `AC_SUBST(ERTS_INTERNAL_X_LIBS)`.
- `ERL_FIND_ETHR_LIB` — the largest macro (lines ~1634–2516), ERTS-only. Adds options `--disable-native-ethr-impls`, `--enable-x86-out-of-order`, `--enable-prefer-gcc-native-ethr-impls`, `--enable-trust-gcc-atomic-builtins-memory-barriers`, `--with-libatomic_ops`, `--with-sparc-memory-order`, `--enable-ppc-lwsync-instruction`, `--without-threadnames`; `AC_SUBST`s `ETHR_X_LIBS ETHR_LIBS ETHR_LIB_NAME ETHR_DEFS ETHR_THR_LIB_BASE ETHR_THR_LIB_BASE_DIR ETHR_X86_SSE2_ASM`.
- `ETHR_CHK_GCC_ATOMIC_OP__`, `ETHR_CHK_IF_NOOP`, `ETHR_CHK_GCC_ATOMIC_OPS`, `ETHR_CHK_INTERLOCKED` — ethread atomics probing helpers; `ETHR_CHK_GCC_ATOMIC_OPS` is also invoked from `lib/erl_interface/configure.ac`.

Time sources
- `ERL_MONOTONIC_CLOCK(level, clock-ids, use_elapsed)` — sets `erl_monotonic_clock_func/_id/_lib/_low_resolution` (clock_gettime / mach_clock_get_time / gethrtime / times / none).
- `ERL_WALL_CLOCK([level [, clock-id]])` — sets `erl_wall_clock_func/_id`.
- `ERL_TIME_CORRECTION` — options `--with-clock-resolution`, `--with-clock-gettime-realtime-id`, `--with-clock-gettime-monotonic-id`, `--enable-prefer-elapsed-monotonic-time-during-suspend`, `--enable-gettimeofday-as-os-system-time`; defines `OS_SYSTEM_TIME_USING_CLOCK_GETTIME` / `OS_SYSTEM_TIME_USING_MACH_CLOCK_GET_TIME` / `OS_SYSTEM_TIME_GETTIMEOFDAY`, `WALL_CLOCK_ID(_STR)`; `AC_SUBST(LIBRT)`.

Security hardening
- `ERL_OSSF` (default on except win32, `--disable-security-hardening-flags`), `ERL_OSSF_CFLAGS/VAR,LDFLAGS`, `ERL_OSSF_CXXFLAGS`, `ERL_OSSF_LDFLAGS`, `ERL_OSSF_FLAGS` (all of the above) — OSSF compiler-hardening flag probing.

Dynamic Erlang Drivers (DED)
- `ERL_DED` = `LM_CHECK_THR_LIB` + `DED_THR_DEFS` (`AC_SUBST`) + `ERL_DED_FLAGS`.
- `ERL_DED_FLAGS` — computes DED compile/link settings per `host_os`/`host_cpu`: `AC_SUBST`s `DED_CC DED_GCC DED_EXT DED_SYS_INCLUDE DED_INCLUDE DED_BASIC_CFLAGS DED_CFLAGS DED_STATIC_CFLAGS DED_WARN_FLAGS DED_WERRORFLAGS DED_LD DED_LDFLAGS DED_LD_FLAG_RUNTIME_LIBRARY_PATH DED_LIBS DED_OSTYPE`; honors externally supplied `DED_LD*`/`ERL_DED_FLAT_BUNDLE` (darwin `-bundle_loader beam.smp`).
  Interface consumed by `make/otp_ded.mk.in` (`@DED_CC@ @DED_LD@ @DED_LDFLAGS@ @DED_LD_FLAG_RUNTIME_LIBRARY_PATH@ @DED_EXT@`) and by `Makefile.in` (`INST_FORBID += $(notdir $(DED_CC)) $(notdir $(DED_LD))`).

Consumers (who includes otp.m4): `make/configure.ac`, `erts/configure.ac`, `lib/{crypto,erl_interface,common_test,megaco,odbc,snmp,wx}/configure.ac`, `lib/common_test/test_server/configure.ac`. Heaviest users: `erts` (EMU_CC, ETHR, TIME_CORRECTION, HARDWARE_ARCH, WINDOWS_ENVIRONMENT) and `lib/erl_interface` (DED, HARDWARE_ARCH, OSSF).
Note: `ERL_DETERMINISTIC` (`@ERL_DETERMINISTIC@` in `make/otp.mk.in`) is **not** defined here — it comes from `erts/configure.ac`.

## Constraints
- Never edit `config.guess`, `config.sub`, `install-sh` — third-party GNU files, tracked by `vendor.info`.
- `otp.m4` is one global m4 namespace shared by every `configure.ac`: a new/changed macro changes configure behaviour tree-wide. Check all consumers before altering an existing macro; prefer adding new macros over changing semantics.
- Changing `otp.m4` requires regenerating the committed `configure` scripts: edit `otp.m4`, then run `./otp_build update_configure`. Never hand-edit a generated `configure`.
- New macros: define with the bracketed form `AC_DEFUN([NAME], [...])` (older macros use unquoted `AC_DEFUN(NAME,`, which still works but should not be imitated) and quote arguments; prefix `LM_` for generic, `ERL_` for Erlang-specific.
- Macros expand into every consumer's `configure`, so they must be plain portable `sh`/autoconf: use `AC_CACHE_CHECK`/`AS_IF`/`AS_CASE`, keep `m4_ifelse`/`m4_default` optional-argument handling, and `AC_SUBST` any variable the Makefiles need.
- Cross-compilation safety: never rely on `AC_RUN_IFELSE`; consult `erl_xcomp_*` (`LM_PRECIOUS_VARS` declares them) or the precomputed cache.
- Register external knobs via `LM_PRECIOUS_VARS` (`AC_ARG_VAR`) so they survive in `config.status`.
- Use `LM_LOG('...')` for debug output instead of echoing to stdout.

## Notes for Agents
- `m4_include([otp.m4])` resolves because `otp_build` runs `autoconf -B "$ERL_TOP/make/autoconf"` (autom4te prepend-include); nothing is copied into app dirs.
- `vendor.info` here describes only the three GNU scripts; `path` is a *list of files* (vs. a folder in other vendor.info files). It is read by `scripts/license-header.es` (`get_vendor_paths` — files under `path` are exempt from the OTP header/license check) and by `.github/scripts/otp-compliance.es`, which emits an SPDX package per `vendor.info` and converts the `annotation` field (`"Vendor package modified in Erlang/OTP"`) into an SPDX `REVIEW` annotation. Other fields: `ID` must be `<SPDX-TOP-LEVEL-PACKAGE>-<VENDOR-ID>`; see `HOWTO/SBOM.md`.
- `win*.config.cache.static` are hand-maintained precomputed `ac_cv_*` caches for configuring OTP on/for Windows (`otp_build build_win*` exports `OVERRIDE_CONFIG_CACHE_STATIC`; the top-level `configure` copies the file into each app dir as `local.static.config.cache` and passes it via `--cache-file`). They go stale silently when configure tests change — update them together with `otp.m4`/`configure.ac` changes (`git log -- make/autoconf/win64.config.cache.static`). Plain `win64.config.cache` / `win32.config.cache` referenced by `otp_build` are **not** in the repo (locally generated). These `*.config.cache.static` files are exempt from license scanning (`config\.cache\.static$` in `scripts/license-header.es`).
- `erts/autoconf/` holds mirror copies of the GNU scripts (kept only for external scripts; `otp_build` refreshes them) and its own `vendor.info` — do not treat it as a second source of truth.
- `ERL_FIND_ETHR_LIB` and `ERL_TIME_CORRECTION` make up the bulk of the file (~1200 lines) and are ERTS-specific; don't refactor them casually. `LM_CHECK_THR_LIB` / `LM_DECL_*` / `LM_SYS_*` are the shared, app-agnostic macros.

## Dependencies
- GNU Autoconf **2.72** (`autoconf`, `autoheader`, `autom4te`); `otp_build` hard-fails on a wrong `autoconf --version`. `autoconf -B` is used to resolve `otp.m4`.
- POSIX shell/`m4`; nothing here is part of the built/installed product.

## See Also
- `../CONTEXT.md` — shared make infrastructure (`otp.mk.in`, `otp_ded.mk.in`) consuming the DED/`@VAR@` outputs.
- `../configure.ac` → `Makefile.in` — the top-level configure (defines `ERL_TOP`, `TARGET`, `CROSS_COMPILING`, `BOOTSTRAP_ONLY`).
- `../../configure.src` / `../../configure` — the parallel-configure driver and the static-cache plumbing.
- `../../otp_build` — `update_configure`, autoconf version check, Windows/static-cache environment.
- `../../scripts/license-header.es`, `../../.github/scripts/otp-compliance.es`, `../../HOWTO/SBOM.md` — `vendor.info` consumers.
- `../../erts/autoconf/`, `../../lib/common_test/test_server/` — mirror copies of the GNU aux scripts.

## Status
Complete — inventory and conventions captured for `otp.m4`, `vendor.info` and the Windows static caches.
