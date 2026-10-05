# CHANGE.md — Major Modifications

This file records **major modifications** to the ERTS Rust rewrite: architectural shifts,
major data-structure refactorings, and algorithmic changes. It is a coarse, human-readable log
of *decisions and structural changes*, complementing `git log` (which holds every line-level change).

Format: one entry per major change — date, area, what changed and why. Newest first.

---

## 2026-09-01 — Milestone 1: crate tree, frozen term layout, typed-IR engine, ETS/GC/messaging interiors

- **Area:** `erts_rust/crates/*`, `erts_rust/tools/*`
- **Change (foundation):** Froze the **term representation** in `beam_term` as a bit-exact port of
  `erts/emulator/beam/erl_term.h`: 2-bit primary tags, 60-bit small integers, 6-bit-shifted atoms,
  header-less 2-word cons cells, boxed sub-tags, and `TAG_LITERAL_PTR` literal pointers. `Eterm` is a
  `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`. All term
  construction/inspection is centralized here (single source of truth for every other crate).
- **Area:** `beam_ets` (Stage 3 data structures)
- **Change (data structures):** ETS keeps the immutable **observable** `ets:*` semantics (key
  equality/ordering, duplicate-key winners, `size`, lookup/select results) while the **interior
  layouts are opened up** (Part II §B/§D). `set`/`bag` use a bucket hash index; `ordered_set` uses a
  key-sorted interior — the legacy **AVL tree is replaced** by a sorted structure in M1 and is slated
  to become a **K-ary B+-tree** (Part II §B.3). Object storage is a never-relocating table-owned bump
  arena (the copy boundary); deletion does not yet reclaim blocks (MSO planned).
- **Area:** `beam_gc` (Stage 4)
- **Change (algorithmics):** Implemented the per-process **generational semi-space Cheney copying**
  collector, preserving the **barrier-free** generational invariant (a free consequence of term
  immutability), with a refcounted **MSO** off-heap sweep. MMTk/Boehm are rejected; GenImmix is a
  measurement-gated future option (Part II §D).
- **Area:** `beam_message`
- **Change (algorithmics):** Implemented the `copy_struct` copy engine + send/receive envelope — the
  per-process-heap copy boundary (Part II §D). Messaging across heaps remains copy-based by design.
- **Area:** `beam_loader` + `beam_engine` (Stage 1 — the execution engine)
- **Change (architecture):** The loader lowers `.beam` to a **typed, block-structured IR**; backends
  consume only the IR. This is the pluggable **execution-engine seam** (Part II §A/§F, "Ruby-YJIT
  shaped"). A **direct-threaded interpreter** implements an honest generic-opcode subset; the
  **BeamAsm-style baseline JIT** and the **optional Cranelift hot tier** sit behind the same seam and
  are measurement-gated / off the critical path. Root-set contract kept: `x_regs[0..live)` + the whole
  stack, **no frame liveness maps**.
- **Area:** `beam_dist`, `beam_abi`
- **Change:** Distribution protocol v6 (`DFLAG_*`/`DOP_*`, handshake, control framing, `epmd` client)
  and the layout-frozen NIF (`enif_*`) / driver (`driver_*`/`erl_drv_*`) C-ABI surfaces, with
  static size/offset assertions and `Eterm ↔ ERL_NIF_TERM` transmutes.
- **Area:** `beam_rt`
- **Change:** The runtime binary (boot, CLI flags, `system_info`) wires the engine + BIFs + process
  context and executes a real `.beam` end-to-end (the vertical slice): `beam_rt run slice.beam
  slice:add/2 2 3` → `5`, `slice:fib/1 10` → `55`.
- **Area:** build system
- **Change:** Developer-iteration build configuration: `mold`/`lld` linker, `opt-level=1` dev profile
  with dependencies at `opt-level=2`, thin-LTO release, and a Nix flake pinning the toolchain plus the
  oracle/target runtimes. `scripts/cargo.sh` selects a writable `CARGO_HOME` for read-only-`$HOME`
  sandboxes.
- **Area:** `tools/differential`, `tools/bench`
- **Change:** The differential compatibility gate defines a **capability-gap contract** (runtime exits
  `3` with `beam_rt: unsupported:` for an unimplemented opcode/BIF/`-eval` form) distinct from a real
  runtime error, so an incapable runtime is reported `unsupported` (honest), never `mismatch`; new
  module-run cases give real oracle-vs-Rust agreement. The benchmark driver gains a **read-only
  target adapter** (build in a `$TMPDIR` copy of a read-only foreign repo) and a **dependency-free
  HTTP/1.1 load generator**, so it measures Cowboy, the Elixir/`erlc` compilers, and the system-ERTS
  baseline with real numbers.

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
  configures a fast linker and fast dev build profiles for iteration speed.
