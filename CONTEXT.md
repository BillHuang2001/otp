# Erlang/OTP — Repository Root

## Intent
Monorepo for Erlang/OTP (release `30.0-rc0` per `OTP_VERSION`; ERTS runtime version `17.1`). Contains the runtime system (`erts/`), the OTP application libraries (`lib/`), the shared build system, system documentation and release/CI tooling. Everything builds in place from this directory (`$ERL_TOP`) via `./configure && make`.

## API Surface (top-level build & release entry points)
- `configure` — wrapper that runs the per-directory autoconf `configure` scripts (`make`, `lib`, `lib/*`, `erts`) in parallel.
  Generated from `configure.src`; edit the `.src` and run `./otp_build update_configure` (never edit `configure` directly).
- `Makefile.in` → `Makefile` — top-level build driver (`make`, `make install`); handles `DESTDIR` / `EXTRA_PREFIX` install prefixing.
- `otp_build` — main helper: bootstrap, `update_configure`, `check`, `release`, `save_bootstrap`, `update_preloaded`, cross-compile (`--xcomp-conf`), Windows (`env_win32`).
- `otp_patch_apply` — apply OTP patch (security/critical) sets.
- `OTP_VERSION` — current OTP release string. `otp_versions.table` (generated) — maps each released OTP version to its component application versions.
- `erl-build-tool-vars.sh` — build-tool env vars recognized by the `configure` scripts (keep in sync with `make/autoconf/otp.m4` and `xcomp/`).
- `TAR.include` — historical source-tarball file list (no in-repo script consumes it).
- `prebuild.skip` / `prebuild.delete` — control packaging of the *prebuilt* source archive: paths in `prebuild.skip` (`Makefile`, `bin`, `bootstrap/bin`, `ibin`) are restored from source; paths in `prebuild.delete` (`bootstrap/lib`, `bootstrap/target`, `.git`, `.github`, `scripts`) are removed.
- `sys.config` — default system configuration (empty `[]`).
- `AUTHORS`, `README.md`, `CONTRIBUTING.md`, `LICENSE.txt` (Apache-2.0), `SECURITY.md`.

## Build Order & Flows
- Configure/build order: `make` → `lib` (`stdlib sasl kernel compiler` first, then the rest) → `erts` (configured and built last, depends on `make/` and `lib/`).
- Docs: `make docs` builds `lib` → `erts` → `system/doc` → `system/doc/top`; rendered by ExDoc (version pinned in `make/ex_doc_vsn`).
- Typical dev loop: `./otp_build configure && make`; per-app `cd lib/<app> && make`; tests via `make test` / `make <app>_test`.

## Constraints
- Never hand-edit generated files (`configure`, `Makefile`, `make/output.mk`, `bootstrap/lib/*`); edit sources/templates and regenerate (`./otp_build update_configure`, `save_bootstrap`, `update_preloaded`).
- Root-level edits must not leak worktree/absolute paths into the build.

## Notes for Agents
- `bin/` and `plt/` are git-ignored build-output directories (only `.gitignore` tracked): `bin/` holds installed release executables plus host binaries in `bin/$(TARGET)/`; `plt/` holds Dialyzer PLT files.
- Toolchain for the top-level build: GNU autoconf 2.72 and make (`otp_build` sets `USE_AUTOCONF_VERSION=2.72`).

## Routing Table
- `erts_rust/` → Rust ERTS rewrite (Cargo workspace): the new BEAM runtime implementation (term/GC/engine/BIFs/ETS/IO/dist/ABI), the differential-test harness, and the external benchmark driver; follows the staged plan (Stage 0–6)
- `erts/` → Erlang Runtime System (BEAM emulator/JIT, epmd, preloaded modules, runtime C libs, exported headers, launcher, system tests); legacy C/C++ tree retained as the rewrite's differential oracle + `FLAVOR=emu` bootstrap fallback
- `lib/` → OTP applications (35 apps: kernel, stdlib, compiler, crypto, ssl, ssh, mnesia, common_test, …)
- `make/` → shared build-system makefile fragments and autoconf helpers (`make/autoconf/`)
- `scripts/` → build/release/CI helper scripts and the git `pre-push` hook
- `bootstrap/` → committed primary bootstrap OTP (kernel/stdlib/compiler beams + boot scripts) used to start a source build
- `system/` → system documentation sources and ExDoc build (`system/doc/`)
- `xcomp/` → cross-compilation configuration files per target platform
- `.github/` → GitHub Actions CI/CD (workflows, composite actions, CI scripts, dockerfiles)
- `HOWTO/` → human-facing build/install/test/porting guides
- `LICENSES/` → third-party license texts (sources for SPDX `LicenseRef-*` ids)
- `FILE-HEADERS/` → SPDX license-header templates enforced by the license scanner
- `.devcontainer/` → VS Code Dev Container / Codespaces environment (toolchain shared with CI and `.gitpod.yml`)
- `.ort/` (with root `.ort.yml`) → OSS Review Toolkit (ORT) SBOM/license-scan configuration used by CI

## See Also
- Root dev-tool config: `.elp.toml` (Erlang Language Platform), `erlang_ls.config`, `renovate.json5`, `.editorconfig`, `.formatter.exs`, `.dir-locals.el`, `.gitpod.yml`.
- `ERTS_RUST_REWRITE_EVALUATION.md` → engineering evaluation of a hypothetical Rust rewrite of ERTS/BEAM (Level 1: BEAM-asm drop-in vs Level 2: source/AST compat), incl. the ERTS compatibility surface, JIT/GC/data-structure/I-O technology survey, the process memory model (messaging/ETS copy boundary), and a recommended incremental strategy. Analytical document only — no code.
- `ERTS_RUST_REWRITE_DEEPDIVE.md` → Part II of the above: deep dives on the core-only-rewrite seam (keep NIFs / reuse C / frozen internal ABI), data structures, GC options in Rust, the process memory model (messaging/ETS copy boundary, what may be shared), compiler optimization feasibility/universality, the JIT (register allocation, backends, tiering) and the interpreter, plus a staged plan. Analytical document only — no code.
- Both rewrite docs are the reference for the **ERTS compatibility surface** any functional-test/benchmark program must exercise (9 contracts: language semantics; `.beam`/asm; NIF & driver C ABIs; distribution wire/ETF; CLI flags; preloaded/boot; introspection; self-hosted build) and for rewrite **seams/risks** (core-only rewrite is a link-time frozen-header boundary; execution-engine + OS/driver are the only clean seams; differential-test harness vs real ERTS is the required safety net). They specify benchmark *categories* (compute/JIT-bound vs messaging/copy-bound vs GC-bound vs I-O-bound) and mandate per-workload measurement and startup/p99-latency budgets, but name **no concrete benchmark suite**.
- `ERTS_RUST_REWRITE_TEST_PROJECTS.md` → short list of *pure* Erlang/Elixir workloads that are popular and fast to benchmark (EMQX, Cowboy, Khepri, the Benchmarks Game kernels), plus a compiler-throughput benchmark, mapped to the rewrite surface they exercise. Fills the "no concrete suite" gap above. Analytical/planning document only — no code.
