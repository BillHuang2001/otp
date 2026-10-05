# erts/autoconf/ — Autoconf auxiliary files (read-only copies)

## Intent
Auxiliary autoconf directory for `erts/`. It holds copies of autoconf's `config.guess`, `config.sub` and `install-sh`, kept only so that external scripts/tooling still looking for them at this historic path keep working.
The primary/master copies live in `../make/autoconf/`.
The real erts autoconf machinery (`configure.ac` → `configure`) lives one level up in `../` (i.e. `erts/`), **not** here.
This is a leaf: no subdirectories, no source code, no build steps of its own.

## API Surface
Tracked files (this is the entire directory):
- `README.md` — states that all files except the README are copies of `$ERL_TOP/make/autoconf`, are updated automatically by `$ERL_TOP/otp_build update_configure [--no-commit]`, are kept only for backwards compatibility, and will eventually be removed.
- `config.guess`, `config.sub`, `install-sh` — byte-identical copies of `../make/autoconf/<same>` (autoconf 2.72 aux scripts; executable).
- `vendor.info` — SPDX/SBOM vendor record (`ID: erts-autoconf`, autoconf 2.72) describing those three scripts. NOT a copy of `../make/autoconf/vendor.info` (which has `ID: make-autoconf`, a different `path` and a reordered `licenseDeclared`).

Consumers / references (verified in this checkout):
- `../../Makefile.in:180` — `TARGET_HOST=$(shell $(ERL_TOP)/erts/autoconf/config.guess)` (cross-compiling branch). Removing `config.guess` breaks the top-level cross build until that reference is updated.
- `../../scripts/build-otp-tar:566` — falls back to `erts/autoconf/config.guess` for the target directory name when `make/autoconf/config.guess` is absent (e.g. old prebuilt source trees).
- `../../otp_build:26` — `autoconf_aux_dirs="./erts/autoconf ./lib/common_test/test_server"`; `update_configure` copies the `make/autoconf` masters of `install-sh`/`config.guess`/`config.sub` into these aux dirs.
- `../../HOWTO/SBOM.md:229` — cites `erts/autoconf/vendor.info` as an example vendor record.
- Several `../../xcomp/*.conf` comments still name `$ERL_TOP/erts/autoconf/config.guess|config.sub` (documentation only; most now point at `make/autoconf`).

## Constraints
- Treat `config.guess`, `config.sub`, `install-sh` as read-only generated copies: do not hand-edit; edit the masters in `../make/autoconf/` and run `./otp_build update_configure [--no-commit]` from `ERL_TOP`.
- `README.md` and `vendor.info` are NOT refreshed by `otp_build update_configure` and are maintained directly here.
- References must keep working: `../../Makefile.in` and `scripts/build-otp-tar` read `config.guess` from this path, so any future removal must update those references first.

## Design Decisions
- The copies exist purely for backwards compatibility with external scripts hard-coding this historic path; the stated upstream intent (see `README.md`) is to remove them eventually and standardise on `../make/autoconf/`.
- `update_configure` copies only the three aux scripts into the aux dirs — it does not touch `vendor.info`, which is why the two `vendor.info` files differ.

## Known Issues
- The `../../Makefile.in` `configure` and `makefiles` convenience targets run `cd autoconf && $(MAKE)` from `erts/`. This directory contains **no** `Makefile`, and nothing in the build generates one here, so running `make configure` or `make makefiles` from `erts/` aborts with make's "No targets specified and no makefile found" error. These targets are explicitly labelled developer convenience shortcuts ("don't use them in scripts") and are stale — historical `erts/autoconf` held its own configure machinery that has since moved to `../make/autoconf/` and `../configure.ac`.
- Stale comments still say aux programs live here even though the code does not: `../../make/configure.ac:89` ("Aux programs are found in erts/autoconf") vs its `AC_CONFIG_AUX_DIRS([${ERL_TOP}/make/autoconf])`, and `erts/configure.ac:74` (`AC_CONFIG_AUX_DIR([${ERL_TOP}/make/autoconf])`).

## Notes for Agents
- If you need the real autoconf support files, use `../make/autoconf/` — that is the source of truth (`otp.m4`, `win{32,64}*.config.cache.static`, etc.). Do not edit files here to change behaviour; edits are overwritten by `otp_build update_configure`.
- `vendor.info` is JSON-with-`//`-comments consumed by the SBOM/license tooling; every entry in its `path` array must point at a file that exists in this directory.

## Dependencies
- GNU autoconf 2.72 (version of the bundled aux scripts; `otp_build` pins `USE_AUTOCONF_VERSION=2.72`).

## Test Strategy
- No dedicated tests. Correctness is exercised indirectly: a successful top-level `./configure`, and cross builds that use `TARGET_HOST`, run `config.guess`/`install-sh` from this path.

## Routing Table
- None — this node is a leaf with no child directories.

## See Also
- `../make/autoconf/` → master copies of these files plus `otp.m4`, `win*.config.cache.static` (source of truth; read-only)
- `../../otp_build` → `update_configure` (copies masters here), `USE_AUTOCONF_VERSION`
- `../../Makefile.in` → `TARGET_HOST` (uses `erts/autoconf/config.guess`)
- `../../HOWTO/SBOM.md` → describes the `vendor.info` SBOM format
