# `src/nif/pending_tests/` — integration tests awaiting a `tests/`-scoped move

## Intent
**Staging only — nothing here is compiled or part of any module.** These are the verified integration
tests for the three `beam_abi` seams delivered by the A.3 ledger round
(`erts_rust/tools/differential/divergences.md` § "A.3 — NIF C ABI: the stateful `enif_*` family"):
`NifHost::binary_to_term_used` behind `enif_binary_to_term`, `NifHost::monitor_process_result`
behind `enif_monitor_process`, and the resource-term release seam `nif::enif::{resource_terms,
release_resource_terms}`.

They live here because the authoring node (`erts_rust/crates/beam_abi/src/nif`) has no write scope
over `erts_rust/crates/beam_abi/tests/`. Cargo only builds `tests/` at the package root, so this
directory is inert: `cargo build --all-targets` and `cargo test -p beam_abi` neither read nor
compile it.

## What the move must do
1. `nif_bin2term.rs` / `nif_monitor_codes.rs` / `nif_resource_release.rs` → `erts_rust/crates/beam_abi/tests/`
   (they `mod common;`, which resolves against `tests/common/mod.rs` — that module must stay reachable).
2. `fixtures/nif_bin2term.c` / `fixtures/nif_monitor_codes.c` / `fixtures/nif_resource_release.c`
   → `erts_rust/crates/beam_abi/tests/fixtures/`.
3. Delete this directory, and drop the `src/nif/pending_tests/` routing entry from `src/nif/CONTEXT.md`.
4. Run `../../../scripts/cargo.sh test -p beam_abi --test nif_bin2term --test nif_monitor_codes --test nif_resource_release`
   and confirm **no `SKIP:` banner** — the oracle halves must really execute (`erl`/`erlc`/`cc` on `PATH`).

## Notes for Agents
- The fixtures reference only `enif_*` slots this crate populates `Some`, so `dlopen(RTLD_NOW)` resolves.
- A NIF-loading test binary must export its `enif_*` symbols: `build.rs` emits
  `cargo::rustc-link-arg-tests=-Wl,--export-dynamic`, which is why these tests work inside this package
  and fail with `undefined symbol: enif_inspect_binary` anywhere that omits it.
- Their expected oracle values are the measured OTP 29.1.1 / erts-17.1 answers — see
  `src/nif/CONTEXT.md` → *Test Strategy*.
- The `.so` files a fixture run produces go to `$TMPDIR` (`compile_so`), never into the repository.
