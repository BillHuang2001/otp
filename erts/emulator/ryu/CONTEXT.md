# erts/emulator/ryu — vendored Ryu float→string

## Intent
Provides the VM's default implementation of shortest round-trip double formatting (`d2s`) for `float_to_list(F, [short])` — the `d2s_buffered_n` path in `erts/emulator/beam/bif.c`. Vendored from Ulf Adams' [ryu](https://github.com/ulfjack/ryu); compiled into `libryu.a` that links into the emulator. When `--disable-builtin-ryu` is used, the VM falls back to C++17 `std::to_chars` in `erts/emulator/sys/common/erl_float_short.cpp` instead (that file `#error`s if `ERTS_USE_BUILTIN_RYU` is set — the two are mutually exclusive).

## Status
Two vendored packages plus OTP glue. Do **not** hand-edit the vendored sources; they carry `//CHANGE_FOR_ERLANG` markers and are refreshed by `update.sh`.
- **`erts-ryu`** (Ulf Adams' ryu) — `common.h`, `d2s.c`, `d2s_full_table.h`, `d2s_intrinsics.h`, `digit_table.h`, `ryu.h`, `LICENSE-Apache2`, `LICENSE-Boost`. sha `4c0618b0…`, license `Apache-2.0 OR BSL-1.0`.
- **`ryu-to_chars`** (Microsoft STL `stl/inc/xcharconv_ryu.h`) — `to_chars.h`, a C-ified `__to_chars`. Repo sha `37d575ede5…`, `versionInfo` `msvc-build-tools-14.50`, license `Apache-2.0 WITH LLVM-exception AND BSL-1.0`. `xcharconv_ryu.h.sha` records the upstream commit of that specific file (`608036cc…`), not the repo HEAD.

## OTP-side files (safe to maintain)
- `ryu.mk` — build include; only `RYU_FILES = d2s` is compiled (`d2s.c` pulls in every other header, incl. `to_chars.h`). Produces `RYU_OBJDIR/$(TYPE)/libryu.a` (or `ryu.lib` on win32).
- `update.sh` — refresh script (needs `GITHUB_TOKEN`; merges `ulfjack/ryu` into `erlang/ryu`, re-fetches the STL file, bumps `vendor.info`) — run from `$ERL_TOP`.
- `README.ryu_update.md` — the three OTP modifications (minimal file set, STL `to_chars` for final string, everything else deleted) and the update procedure.
- `vendor.info` — SPDX/REUSE metadata for both packages (`annotation: "Vendor package modified in Erlang/OTP"`); both list `update.sh`.
- `xcharconv_ryu.h.sha`, `xcharconv_ryu.h.sha.license` — pinned commit/license for the STL file.

## How to update
Run `./update.sh` in this directory. It clones `erlang/ryu`, merges upstream `ulfjack/ryu`, then compares the STL file commit; if `stl/inc/xcharconv_ryu.h` changed you must manually inspect `__to_chars` in `to_chars.h` for changes that need uplifting, and update `xcharconv_ryu.h.sha`. Note `README.ryu_update.md` and `to_chars.h` cite different upstream STL commits (the header comment is authoritative).

## Build integration
Controlled by `--enable-builtin-ryu` / `ERTS_USE_BUILTIN_RYU` in `erts/configure.ac` (default `yes`; `no` requires C++17 `std::to_chars`). In `erts/emulator/Makefile.in`: includes `ryu/ryu.mk`, adds `-Iryu` to `COMMON_INCLUDES`, `$(RYU_OBJDIR)` to `CREATE_DIRS`, and links `$(RYU_LIBRARY)`; `ERTS_USE_BUILTIN_RYU` is `AC_DEFINE`d so `beam/bif.c` selects `d2s_buffered_n`.

## Constraints
- Must compile as C (`RYU_FILES=d2s`) yet expose `extern "C"` symbols (`ryu.h`) for the C++ emulator.
- `RYU_CFLAGS` filters out `-Wdeclaration-after-statement` (upstream uses C99 mid-block declarations); win32 uses `$(CFLAGS)` verbatim.

## Known Issues
- Vendored files differ from upstream (`//CHANGE_FOR_ERLANG`): zero prints as `"0.0"` not `"0E0"`, the small lookup table was dropped in favour of `d2s_full_table.h`, and the STL buffer-length checks were removed (Erlang always passes a 256-byte buffer, max needed is ~30) — mind this when refactoring.
- `d2s.c` includes `to_chars.h`, so the two vendor packages are compiled as one translation unit.
- `ryu/obj/` is git-ignored build output; empty in a clean checkout.

## See Also
- `../Makefile.in` → build wiring (`ERTS_USE_BUILTIN_RYU`, `RYU_OBJDIR`, `-Iryu`)
- `../beam/bif.c` → consumer (`float_to_list` short format)
- `../sys/common/erl_float_short.cpp` → non-builtin C++17 fallback
