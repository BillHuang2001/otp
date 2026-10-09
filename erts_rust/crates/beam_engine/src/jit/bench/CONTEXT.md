# `beam_engine` JIT measurement harnesses (`src/jit/bench/`)

## Intent
The in-crate, `#[cfg(test)]` benchmark harness for the execution tiers. Its `#[ignore]`d timing test
(`jit::bench::interpreter_versus_native_tiers_measurement_table`) prints the broad
interpreter-vs-baseline tier **table** — `ns/op` + `ops/s` per `(workload, tier)` plus the speed-up
over the interpreter — for the four opcode shapes the baseline tier does **not** inline
(`build`/`map`/`select`/`walk`, i.e. every one of them is a `jit_step_one` round-trip on a native
tier). The **non-ignored** agreement test
(`jit::bench::the_interpreter_and_the_baseline_tier_agree_on_every_workload`) keeps the correctness
half in the ordinary `cargo test` run.

## API Surface (crate-visible; `crate::jit::bench::…`)
- `support` (`pub(crate) mod`) — the scaffolding shared with the sibling Cranelift harness
  (`crate::jit::cranelift::bench`): `arith_hook(truthy, falsy) -> BifHook` (small-int `erlang`
  `+`/`-`/`*` and the eight comparison operators), the synthetic-`Module` builder `Fixture`
  (`new`/`with_plus`/`import`/`add`/`finish`, plus a feature-gated `intern`), `Samples`
  (feature-gated; `min`/`median`) and the report primitives `ns_per_op`/`ops_per_sec`.
- `workloads` — the four workload definitions (`Workload`: module + exported `function/arity` +
  argument source + a value check pinning the intended result), their round constants
  (`BUILD_ROUNDS`/`MAP_ROUNDS`/`SELECT_ROUNDS`/`WALK_LENGTH`, `TIMED_RUNS`/`AGREEMENT_RUNS`) and the
  in-context argument builders (`map_args`/`walk_args`).
- `checks` — the per-workload result checks (`verify_build`/`verify_map`/`verify_select`/`verify_walk`).
- `mod.rs` — the tier seam (`Runner`), the timing loop (`Measurement`) and the reporting
  (`report`/`bench_workload`), plus the two tests.

## Constraints
- Test-only: wired by `#[cfg(test)] mod bench;` in `../mod.rs`; nothing here ships in the engine.
- Uses only the crate's own seams (`JitBackend`, `Machine::run_export`, `MinimalContext`); a native
  tier is compiled **once** per measurement (outside the timed loop) and only `run_compiled` is timed.
- `support` is the **single** home for the scaffolding both in-crate harnesses share; do not re-add a
  private copy in either harness. `Samples` and `Fixture::intern` are feature-gated on `jit-cranelift`
  so the feature-off build carries no dead code.
- Measurement strategy is **not** shared: this module's fixed-order tier table and the ABBA paired
  ratio of `../../cranelift/bench.rs` stay separate.

## Test Strategy
- `#[ignore]`d timing table:
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib -- --ignored --nocapture`
  (run it explicitly — CI does not run `#[ignore]`d tests).
- The agreement test runs in the plain suite.
- Numbers are wall-clock on a shared sandbox (order-of-magnitude evidence only); correctness — each
  tier agreeing on the workload's *intended* value — is the only thing asserted, never "the JIT is faster".

## Routing Table
- `support.rs` → the shared scaffolding (see API Surface); `workloads.rs` → the four workloads;
  `checks.rs` → the result checks; `mod.rs` → the tier-table harness.
- `../../cranelift/bench.rs` → the sibling order-balanced baseline-vs-Cranelift harness, which
  consumes `support` through `crate::jit::bench::support`.

## See Also
- `../../CONTEXT.md` → the JIT / execution-tier seam; `../../cranelift/CONTEXT.md` → the Cranelift harness.
