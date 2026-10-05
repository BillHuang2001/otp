# erts/emulator/zlib/ — vendored zlib

## Intent
Upstream zlib compression library vendored as a private copy that is compiled into the BEAM emulator when no suitable system zlib is available.
Provides the compression primitives behind the preloaded `zlib` Erlang module (NIF) and ERTS internals (term compression, `crc32`/`adler32`, compressed BEAM files, distribution handshake).
Follows the same `vendor.info` scheme as the sibling vendored 3PPs `../asmjit/`, `../pcre/`, `../zstd/`, `../ryu/`.

## Status: third-party, do not modify
- `https://github.com/madler/zlib`, version `1.3.2`, license `Zlib` (SPDX), supplier Mark Adler, upstream sha `da607da739fa6047df13e66a2af6b8bec7c2a498` — all in `vendor.info`.
- The `.c`/`.h` files are pristine upstream source (no OTP copyright/SPDX headers, no local patches); vendored code is exempt from the OTP header per `HOWTO/FILE-HEADERS.md`.
- Only `zlib.mk` and `vendor.info` are OTP-authored, and both are listed in `vendor.info`'s `exclude` list.

## How to update
No `update.sh` here and no `update` key in `vendor.info` (unlike `../zstd/`, `../ryu/`, which Renovate drives through `.github/scripts/renovate-vendored-deps.sh` + `.github/workflows/renovate-vendored-deps.yaml`).
Refresh by hand: copy the new upstream file set in, update `versionInfo`/`sha`/`copyrightText` in `vendor.info`, keep `ZLIB_FILES` in `zlib.mk` in sync with the files actually present, and update `ERTS_USE_BUILTIN_ZLIB` usage in `../beam/erl_bif_info.c` (`erlang:system_info(embedded_3pps)`) — see `HOWTO/SBOM.md`.

## Build integration
- `zlib.mk` compiles `ZLIB_FILES` (adler32, compress, crc32, uncompr, deflate, trees, zutil, inflate, inftrees, inffast) into `$(ZLIB_OBJDIR)` = `erts/emulator/zlib/obj/$(TARGET)/$(TYPE)` (git-ignored, with `zlib/win32/`) and archives them to `libz.a` (unix) / `z.lib` (win32). The gzip file-I/O layer (`gz*.c`) is not vendored.
- `../Makefile.in` includes `zlib.mk`, adds `$(ZLIB_OBJDIR)` to `CREATE_DIRS`, and only when `Z_LIB` is empty adds `-Izlib` to `COMMON_INCLUDES` and links `$(ZLIB_LIBRARY)`.
- Configure knob (`erts/configure.ac`): `--enable-builtin-zlib` forces this copy; `--disable-builtin-zlib` (effectively the default) probes for a system zlib >= 1.2.5 and sets `Z_LIB=-lz`, falling back to builtin — which clears `Z_LIB` and defines `ERTS_USE_BUILTIN_ZLIB`. `--enable/--disable-use-embedded-3pp-alternatives` toggles this together with ryu/zstd.
- Also wired into dependency/compdb generation from `$(ZLIB_SRC)`: `$(TTF_DIR)/zlib.depend.mk` and `zlib.json` (part of `compile_commands.json`).

## Consumers in-tree (all reach this dir through the same headers)
- `../beam/erl_zlib.h` is the wrapper: it includes `<zlib.h>`, resolving to this directory via `-Izlib` in builtin builds or to the system header otherwise.
- ERTS internals include the wrapper: `../beam/erl_zlib.c` (OTP-adapted `compress2`/`uncompress` using erts allocators — OTP code, not vendored) plus `erl_bif_chksum.c`, `beam_file.c`, `beam_load.c`, `external.c`, `erl_bif_re.c`, `dist.h`.
- The `zlib` Erlang module: `../nifs/common/zlib_nif.c` (`STATIC_ERLANG_NIF` compiled into the emulator) backs preloaded `erts/preloaded/src/zlib.erl`. So this directory serves both the NIF and the BIFs — there is no separate zlib used by the NIF.

## Known Issues
- Only compiled in a builtin-zlib build; with a system zlib (`Z_LIB` set) these sources are unused, `-Izlib` is not added, and the C files here are not compiled at all.
- `ZLIB_CFLAGS` strips `-Werror=undef` and, on win32, a separate zlib copy is deliberately required for debug builds; `TYPE=debug` adds `-DZLIB_DEBUG=1`.

## Notes for Agents
- Never `#include <zlib.h>` directly in emulator code; include `../beam/erl_zlib.h` instead — it also supplies the `HAVE_UNISTD_H`/`HAVE_STDARG_H` workaround needed to compile zlib.h under `-Werror=undef`.
- `zlib.h` in this dir is the only thing defining the public API; `crc32.h` is a large generated table, not hand-editable.

## See Also
- `../Makefile.in` → `Z_LIB`/`ZLIB_LIBRARY` wiring, `COMMON_INCLUDES`, `CREATE_DIRS`, depend.mk/compdb targets
- `../beam/erl_zlib.h`, `../beam/erl_zlib.c`, `../nifs/common/zlib_nif.c`, `../../preloaded/src/zlib.erl` → the actual users
- `../zstd/`, `../ryu/` → sibling vendored libs (with `update.sh`, for the vendor-update pattern)
- `../../../HOWTO/SBOM.md`, `../../../HOWTO/FILE-HEADERS.md` → vendor.info and license-header rules
