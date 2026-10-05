# erts/emulator/zstd/ — vendored Zstandard (zstd)

## Intent
Vendored copy of Facebook/Meta **Zstandard** (`zstd`) providing the compression backend for the `zstd` Erlang module (`../../../lib/stdlib/src/zstd.erl`) via the static NIF `../nifs/common/zstd_nif.c`.
It is the only consumer: `ZSTD_*` symbols / `erl_zstd*.h` appear nowhere else in the tree, so this directory exists solely for the `zstd` NIF (no internal VM/crash-dump use).
Built into `libzstd.a` (or `zstd.lib` on win32) and linked statically into `beam`.

## Status
Third-party code — **do not hand-edit** (upstream sources plus OTP glue are mixed in the same directory).
- Version `v1.5.7`, license `BSD-3-Clause OR GPL-2.0-only`, upstream `https://github.com/facebook/zstd`, sha `f8745da6ff1ad1e7bab384bd1f9d742439278e99` (see `vendor.info`, `zstd.version`).
- `common/`, `compress/`, `decompress/` + `COPYING`, `LICENSE` are upstream files; `zstd.decompress/huf_decompress_amd64.S` is used on non-win32 only.
- OTP-side files (listed in `vendor.info`'s `exclude`): `zstd.mk`, `update.sh`, `vendor.info`, `zstd.version`, plus the three glue headers below.

## OTP glue headers
Renamed copies of the upstream API headers so they cannot clash with a system `libzstd`'s `<zstd.h>`:
- `erl_zstd.h` (from `lib/zstd.h`), `erl_zdict.h` (from `lib/zdict.h`), `erl_zstd_errors.h` (from `lib/zstd_errors.h`).
- Only the include names are rewritten (by `update.sh`), not the symbol names; `zstd_nif.c` picks them via `#ifdef ERTS_USE_BUILTIN_ZSTD` vs `#include <zstd.h>/<zdict.h>`.
- Symbol leakage is prevented at build time with `-DZSTDLIB_VISIBLE= -DZSTDERRORLIB_VISIBLE=` in `zstd.mk` (see `erts/doc/notes.md` on hidden-visibility of the `beam` namespace).

## How to update
`./update.sh` (needs `GITHUB_TOKEN`, `ERL_TOP`, `jq`): `git rm`s everything except `update.sh|vendor.info|zstd.mk|obj`, queries the latest GitHub release tag, clones it, copies `lib/{common,compress,decompress}` + `LICENSE`/`COPYING`, copies and renames the three API headers with `sed` include rewrites, then refreshes `vendor.info` (`versionInfo`/`sha`) and commits.
`zstd.version` holds the pinned upstream sha.

## Build integration
- `zstd.mk` (included unconditionally from `../Makefile.in`) globs the C/`.S` sources, compiles them into `ZSTD_OBJDIR = $(ERL_TOP)/erts/emulator/zstd/obj/$(TARGET)/$(TYPE)` and archives `libzstd.a` (win32: `zstd.lib` via `$(V_AR) -out:`). `ZSTD_OBJDIR` is in `CREATE_DIRS`; `$(ZSTD_SRC)` feeds `$(TTF_DIR)/zstd.depend.mk`.
- CFLAGS: `-O3` normally (`-O2`→`-O3` substitution), `-DDEBUGLEVEL=1` for `TYPE=debug`, `-O0 -fprofile-arcs -ftest-coverage` for `gcov`, `-DZSTD_ADDRESS_SANITIZER` for `asan`; `-MMD -MP` deps.
- Configure knob `--enable-builtin-zstd` / `--disable-builtin-zstd` (default: try the system lib) in `../../configure.ac`, also set by `--enable/--disable-use-embedded-3pp-alternatives`. Requirement for a system lib: static `libzstd` **>= 1.5.6** (`LIBZSTD="-Wl,-Bstatic -lzstd -Wl,-Bdynamic"`, `AC_LINK_IFELSE` probe); `LIBZSTD` is `AC_SUBST`'d into `LIBZSTD=@LIBZSTD@` in `../Makefile.in`.
- `LIBZSTD` set → `LIBS += $(LIBZSTD)`, no `-Izstd`; unset → `LIBS += $(ZSTD_LIB)` (win32 `-L$(ZSTD_DIR) -lzstd`), `DEPLIBS += $(ZSTD_LIB)`, `COMMON_INCLUDES += -Izstd`, and `AC_DEFINE(ERTS_USE_BUILTIN_ZSTD)`.

## Known Issues / Notes for Agents
- The static library is only linked/built when `LIBZSTD` is empty; `zstd.mk` is still included (and its depend-scan runs) in the system-libzstd build.
- `zstd_nif.o` is compiled unconditionally (`NIF_OBJS` in `../Makefile.in`) — `LIBZSTD` selects the headers, never whether the NIF exists.
- Vendored sources are *not* byte-identical to upstream: `update.sh`'s `sed` rewrote their `#include` lines to the `erl_*.h` names.
- Windows uses MSVC while `cc.sh` defines `__aarch64__`; `zstd.mk` therefore adds `-DNO_PREFETCH` (GNU `__asm__` prefetch breaks `cl.exe`).
- `obj/` is a build-output directory and is absent in a clean checkout; `.gitignore` only lists the `libzstd.pc`/`libzstd-nomt` install artifacts.
- Tests live outside this directory: `../../../lib/stdlib/test/zstd_SUITE.erl`.

## See Also
- `../nifs/common/zstd_nif.c` → the NIF implementation and its CONTEXT routing entry (`../nifs/CONTEXT.md`)
- `../../../lib/stdlib/src/zstd.erl` → the Erlang API
- `../CONTEXT.md` → emulator build wiring, other vendored 3PP (`zlib/`, `pcre/`, `ryu/`, `asmjit/`)
