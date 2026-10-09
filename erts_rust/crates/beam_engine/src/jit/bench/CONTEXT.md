# `beam_engine` JIT measurement harnesses (`src/jit/bench/`)

## Intent
The in-crate measurement harness for the execution tiers, over **nine** workloads split by lowering
story. It is a `pub mod` (not `#[cfg(test)]`) because the `#[test]` entry points live in the
integration test `tests/jit_bench.rs`, which cannot see a `#[cfg(test)]` lib module; the library only
exposes two plain functions and the harness itself is never on the engine's execution path.

Nine workloads, split by how the baseline tier lowers them:

* **Deopted** (every instruction is a `jit_step_one` helper round-trip on a native tier) — in
  `workloads.rs`: **`build/0`** (`put_tuple2`/`put_list` term building), **`map/2`**
  (`has_map_fields`/`get_map_elements`/`put_map_assoc`), **`select/1`** (a body dominated by
  `select_val`, deliberately mixing the inlined immediate shape with the deopted register shape) and
  **`walk/2`** (a hot tail-recursive cons-list walk: an `is_nonempty_list` guard + `get_list` + a
  small-int `'+'/2` + `call_only`).
* **Inlined** (the tier handles the body natively) — in `inlined.rs`: **`slice:fib/1`** (a real
  `.beam` fixture), **`kernel/2`**, **`wide/2`**, **`aggr/2`** and **`cmp/2`**.

## API Surface (crate-visible; `crate::jit::bench::…`)
- `support` (`pub(crate) mod`) — the scaffolding shared with the sibling Cranelift harness
  (`crate::jit::cranelift::bench`): `arith_hook(truthy, falsy) -> BifHook` (small-int `erlang`
  `+`/`-`/`*` and the eight comparison operators), the synthetic-`Module` builder `Fixture`
  (`new`/`with_plus`/`import`/`add`/`finish`/`intern`), `Samples` (feature-gated on `jit-cranelift`;
  `min`/`median`) and the report primitives `ns_per_op`/`ops_per_sec`.
- `workloads` — the `Workload` type (module + exported `function/arity` + argument source + a value
  check pinning the intended result), the four deopted workload builders, their round constants
  (`BUILD_ROUNDS`/`MAP_ROUNDS`/`SELECT_ROUNDS`/`WALK_LENGTH`, `TIMED_RUNS`/`AGREEMENT_RUNS`) and the
  in-context argument builders (`map_args`/`walk_args`); `workloads()` returns all nine (the four
  here plus `inlined::inlined_workloads()`). `Workload::bif_hook()` installs the arithmetic hook
  (with working booleans only when the module interns both `true` and `false`).
- `inlined` — the five tier-inlined workload builders and their pinned result constants
  (`FIB_RESULT`/`KERNEL_RESULT`/`WIDE_RESULT`/`AGGR_RESULT`, `pub(super)` for `checks`).
- `checks` — the nine per-workload result checks (`verify_build`/`verify_map`/`verify_select`/
  `verify_walk`/`verify_fib`/`verify_kernel`/`verify_wide`/`verify_aggr`/`verify_cmp`), each
  `fn(&Workload, Eterm) -> Result<(), String>`.
- `mod.rs` — the tier seam (`Runner`), the timing loop (`Measurement`) and the reporting
  (`report`/`bench_workload`), plus the two public entry points `assert_tier_agreement()` (fast, the
  agreement half) and `run_measurement_table()` (the `#[ignore]`d timing table).

## Constraints
- Wired by `pub mod bench;` in `../mod.rs`; the crate-public surface is the two entry functions only
  (the internal `mod`s stay `pub(crate)`/`pub(super)`).
- Uses only the crate's own seams (`JitBackend`, `Machine::run_export`, `MinimalContext`); a native
  tier is compiled **once** per measurement (outside the timed loop) and only `run_compiled` is timed.
- `support` is the **single** home for the scaffolding both in-crate harnesses share; do not re-add a
  private copy in either harness.
- Measurement strategy is **not** shared: this module's fixed-order tier table and the ABBA paired
  ratio of `../../cranelift/bench.rs` stay separate.

## Test Strategy
- `tests/jit_bench.rs` is the thin driver: the agreement `#[test]` runs in the plain suite; the
  `#[ignore]`d timing table prints via
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test jit_bench -- --ignored --nocapture`
  (run one test so the workloads never run concurrently; add `--features jit-cranelift` for the
  Cranelift column).
- Numbers are wall-clock on a shared sandbox (order-of-magnitude evidence only); correctness — each
  tier agreeing on the workload's *intended* value — is the only thing asserted, never "the JIT is
  faster".

## Routing Table
- `support.rs` → the shared scaffolding (see API Surface); `workloads.rs` → the four deopted
  workloads; `inlined.rs` → the five tier-inlined workloads; `checks.rs` → the result checks;
  `mod.rs` → the tier-table harness.
- `../../cranelift/bench.rs` → the sibling order-balanced baseline-vs-Cranelift harness, which
  consumes `support` through `crate::jit::bench::support`.

## See Also
- `../../CONTEXT.md` → the JIT / execution-tier seam; `../../cranelift/CONTEXT.md` → the Cranelift harness.
- `../../../MEASURED.md` → the measured tier table this harness produces.
