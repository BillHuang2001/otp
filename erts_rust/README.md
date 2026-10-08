# Rust ERTS rewrite (`erts_rust/`)

A from-scratch, memory-safe Rust reimplementation of **ERTS** — the Erlang/BEAM runtime — targeting
**full drop-in compatibility** with Erlang/OTP, higher robustness, a modern codebase, and improved
performance (speed and memory).

This workspace is the *new* ERTS core. The legacy C/C++ tree under [`../erts/`](../erts) is retained
only as (a) the **differential-test oracle** and (b) the `FLAVOR=emu` bootstrap fallback, and is
removed module-by-module as Rust parity is proven.

## Design source

The architecture follows, and must stay faithful to, two in-repo documents:

- [`../ERTS_RUST_REWRITE_EVALUATION.md`](../ERTS_RUST_REWRITE_EVALUATION.md) — Part I: the nine
  compatibility contracts, the current-ERTS baseline, and the JIT/GC/data-structure/I-O landscape.
- [`../ERTS_RUST_REWRITE_DEEPDIVE.md`](../ERTS_RUST_REWRITE_DEEPDIVE.md) — Part II: the seams, the GC
  recommendation (per-process generational copying), the messenger/ETS copy boundary, and the
  **staged plan (Stage 0–6)** implemented here.
- [`../ERTS_RUST_REWRITE_TEST_PROJECTS.md`](../ERTS_RUST_REWRITE_TEST_PROJECTS.md) — the concrete
  benchmark/test targets (Khepri, Cowboy, Benchmarks-Game Elixir, EMQX, compiler throughput).

See [`CONTEXT.md`](./CONTEXT.md) for the binding cross-crate contracts and the routing table.
Major structural changes are recorded in [`../CHANGE.md`](../CHANGE.md).

## Layout

```
erts_rust/
├── Cargo.toml            # workspace (15 crates + 2 tools)
├── flake.nix             # Nix dev environment (toolchain + mold + oracle runtimes)
├── rust-toolchain.toml   # pinned Rust toolchain
├── .cargo/config.toml    # mold linker + fast dev profile
├── crates/               # the ERTS implementation
│   ├── beam_common/      # shared error/result/config (lowest common ancestor)
│   ├── beam_term/        # tagged term representation (frozen) + atom table + literals
│   ├── beam_alloc/       # allocators (erts_alloc replacement) + process heap
│   ├── beam_codec/       # External Term Format (ETF) encode/decode
│   ├── beam_dist/        # distribution protocol v6 + epmd + handshake
│   ├── beam_abi/         # NIF + driver C ABI compatibility
│   ├── beam_loader/      # .beam parsing + loader + ops.tab transforms
│   ├── beam_engine/      # typed IR + interpreter + JIT backends (beamasm, cranelift)
│   ├── beam_gc/          # per-process generational copying GC + MSO
│   ├── beam_process/     # processes, scheduler, run queues, thread progress
│   ├── beam_message/     # copy engine + messaging
│   ├── beam_bifs/        # BIF registry + implementations
│   ├── beam_ets/         # ETS tables
│   ├── beam_io/          # poller/reactor + ports + built-in drivers
│   └── beam_rt/          # runtime binary: boot, CLI, introspection, glue
└── tools/
    ├── differential/     # differential-test harness vs real ERTS (the compatibility gate)
    └── bench/            # benchmark driver for the external target projects
```

## Build & test

Fast dev loop (no Nix required if rustc/cargo/mold are on `PATH`):

```sh
cargo build --workspace          # or: cargo b
cargo test  --workspace          # or: cargo t
```

Reproducible environment:

```sh
nix develop                      # toolchain + mold + erlang/elixir/rebar3 (oracle + targets)
```

CI: [`.github/workflows/erts_rust.yml`](../.github/workflows/erts_rust.yml) is path-triggered on
`erts_rust/**` and builds the workspace, runs the workspace tests, runs a non-blocking clippy, and runs
the differential gate ([`tools/differential/gate.sh`](./tools/differential/gate.sh)) against an
Erlang/OTP oracle. Current workspace health and gate numbers live in [`CONTEXT.md`](./CONTEXT.md).

Everything is Apache-2.0, matching upstream OTP. Third-party dependencies are Apache-2.0/MIT.

Every Rust source file carries the OTP Apache-2.0 `%CopyrightBegin%`/`%CopyrightEnd%` header block,
enforced byte-exactly by [`../scripts/license-header.es`](../scripts/license-header.es)
(`ERL_TOP="$PWD" escript scripts/license-header.es scan --path erts_rust`). Add it to every new `.rs`
file — the required text is in [`CONTEXT.md`](./CONTEXT.md) §Constraints.
