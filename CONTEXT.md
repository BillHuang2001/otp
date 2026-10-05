# Erlang/OTP — Repository Root

## Intent
Monorepo for Erlang/OTP (current version `30.0-rc0`, see `OTP_VERSION`). Contains the Erlang runtime system, the OTP library applications, the build system, system documentation and release tooling. Everything is built in place from this top directory (`ERL_TOP`) with `./configure && make`.

## API Surface (top-level build & release entry points)
- `configure` — handcrafted wrapper that runs the per-directory autoconf `configure` scripts in parallel.
  Generated from `configure.src`; edit `configure.src` and run `./otp_build update_configure` (never edit `configure` directly).
- `Makefile.in` → `Makefile` — top-level build driver (`make`, `make install`); handles `DESTDIR` / `EXTRA_PREFIX` install prefixing.
- `otp_build` — helper for bootstrap, `update_configure`, release/tar creation.
- `otp_patch_apply` — apply OTP patch (security/critical) sets.
- `OTP_VERSION` — current OTP release version string. `otp_versions.table` (generated, 500+ lines) — maps every OTP release to its component application versions.
- `erl-build-tool-vars.sh` — build tool env vars recognized by the `configure` scripts (keep in sync with `xcomp/` and `erts/aclocal.m4`).
- `TAR.include` — file list included in the source tarball.
- `prebuild.skip` / `prebuild.delete` — lists controlling prebuilt-artifact handling during bootstrap.
- `sys.config` — default system configuration (empty `[]`).
- `AUTHORS`, `LICENSE.txt` (Apache-2.0), `SECURITY.md`, `CONTRIBUTING.md`, `README.md`.

## Constraints
- Never edit generated files directly (`configure`, `Makefile`); edit their `.src`/`.in` sources and regenerate.
- Root-level edits must not introduce worktree/absolute build paths.

## Notes for Agents
- `bin/` and `plt/` are git-ignored build-output directories (only `.gitignore` is tracked): `bin/` receives installed release executables; `plt/` receives Dialyzer PLT files.
- Toolchain: the top-level build uses GNU autoconf 2.72 and make; `configure` runs sub-configures in parallel.

## Routing Table
- `erts/` → Erlang Runtime System (BEAM emulator, `epmd`, preloaded modules, runtime libs, C sources, system tests)
- `lib/` → OTP applications (kernel, stdlib, compiler, crypto, ssl, ssh, mnesia, … 36 apps)
- `make/` → shared build-system makefile infrastructure (`otp.mk.in`, per-app targets, autoconf helpers)
- `scripts/` → build/release/CI helper scripts (tar creation, license headers, dialyzer runner, pre-push hooks)
- `bootstrap/` → prebuilt bootstrap OTP (beam files + start scripts) used to kick off a source build
- `system/` → system-level documentation (reference manual, guides, `docs.exs` doc build)
- `xcomp/` → cross-compilation configuration files per target platform
- `.github/` → GitHub Actions CI/CD workflows, composite actions and CI scripts
- `HOWTO/` → build/install/test/porting HOWTO documents (INSTALL, TESTING, DEVELOPMENT, …)
- `LICENSES/` → third-party license texts (referenced from SPDX `LicenseRef-*`)
- `FILE-HEADERS/` → SPDX license-header templates used by the license scanner
