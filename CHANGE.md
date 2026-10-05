# CHANGE.md — Major Modifications

This file records **major modifications** to the ERTS Rust rewrite: architectural shifts,
major data-structure refactorings, and algorithmic changes. It is a coarse, human-readable log
of *decisions and structural changes*, complementing `git log` (which holds every line-level change).

Format: one entry per major change — date, area, what changed and why. Newest first.

---

## 2026-09-01 — Bootstrap: Rust ERTS rewrite workspace established

- **Area:** repository root / `erts_rust/`
- **Change:** Introduced a new Cargo workspace, `erts_rust/`, as the from-scratch Rust
  reimplementation of ERTS (the BEAM runtime). It supersedes the legacy C/C++ emulator for the
  new build path; the legacy tree under `erts/` is retained as the **differential-test oracle**
  and the `FLAVOR=emu` bootstrap fallback, to be **removed module-by-module** as Rust parity is
  proven by the differential harness.
- **Why:** The rewrite targets full drop-in compatibility with Erlang/OTP (`30.0-rc0` / `erts-17.1`),
  higher robustness (safe periphery), a modern codebase, and improved speed + memory. The staged
  decomposition (Stage 0–6) comes from `ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and
  `ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II).
- **Structure:** 15 library/binary crates + 2 tooling crates, decomposed along the BEAM subsystem
  seams documented in Part II §A/§H. Cross-crate contracts (frozen term representation, error
  strategy, IR seam, root-set contract, differential-test gate) are fixed at the workspace level in
  `erts_rust/CONTEXT.md`.
- **Tooling:** Nix flake (`erts_rust/flake.nix`) pins the dev environment; `.cargo/config.toml`
  configures the `mold` linker and fast dev build profiles for iteration speed.
- **Data structures / algorithms:** none yet — this entry records the structural bootstrap only.
