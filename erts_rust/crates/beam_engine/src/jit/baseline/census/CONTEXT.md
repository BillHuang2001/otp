# `census` — the baseline tier's deopt, execution and code-size censuses

## Intent
The test-only instruments that rank what the tier still fails to lower and how much code each lowered shape costs: the evidence a widening of the inline set is chosen from.
Both modules are `#[cfg(test)]`-gated at every call site, so they cost a production build nothing.
The figures they produce are recorded in `../measure/CONTEXT.md` (per-lever A/B and the deterministic structural counters) and, at crate level, in `../../../MEASURED.md`.

## API Surface (crate-visible; test builds only)
- `../counters.rs` — `Counters`, its `record_*` call sites, `snapshot()`, and the static deopt maps (`instr_instances`/`instr_deopts`).
  - the `jit_step_one`/`jit_step_ext` deopts, with a `Test` deopt keyed by kind (`"Test(IsEqExact)"`, `"Test(IsTuple)"`, …) in `deopts_by_variant`, so which `test` shapes still deopt is readable per predicate;
  - the driver round-trips;
  - the prologue position/publish calls;
  - the **register-cache traffic** (`record_spill`/`record_fill`/`record_block`: per-block IR/byte/cache/write-set sizes plus spill/fill stores by site), where one recorded run yields all **three** spill **store** models (the **dirty** set `spill` emits via `spill_slots_at(pc)`, the whole-block **written** set and the **full cache**) and both fill models;
  - the **Y-window traffic** (`record_y_access`/`record_y_invalidation`/`record_block`: emitted guarded `Y` loads/stores and their emitted bytes, `Y`-touching blocks, the in-block refresh sites, and the same-block `Y`-slot reuse count);
  - the **static deopt census** (`record_instr_instance`/`record_instr_deopt` plus the `instr_instances`/`instr_deopts` maps driving `../census.rs`).
- `../census.rs` — the four views below, each printing a table; the static corpus census (fixtures plus generically-discovered OTP modules), the runtime hot-shape census (deopts **and**, from the same workloads on the interpreter tier, the `exec_census` per-variant execution ranking plus typed-error table via its `dynamic_execution_census` helper), and the per-kind emitted-byte-size table.
- `../emit/test/refusal_census.rs` (`#[cfg(test)] mod refusal_census` from `../emit/test.rs`) — `the_refusal_attribution_census_is_printed`, attributing `test`/`select_val`/`select_tuple_arity` refusals to exact predicates/operand shapes and listing the `bif`/`gc_bif`/`bif0` deopt pools by BIF name.
  The pools are **self-maintaining**: `already_inlined(module, instr)` consults the emitters' real shape predicates (`comparison_shape`/`fast_arith`/`bif_shape`) rather than a name allowlist, so a newly lowered name leaves its pool automatically (the `bif0` pool has no arm to consult — nothing lowers `Bif0`).

## The four views
- **Static** (`../census.rs::the_static_deopt_census_over_the_corpus_is_printed`): scores the **compile-time** dispatch decision deterministically.
  `counters.rs::record_instr_instance` (the emitter loop's top, `../emit/block.rs`) counts every instruction the lowering dispatch visits per variant, and `counters.rs::record_instr_deopt` — reached through the `../emit/block.rs::emit_deopt` wrapper that **every** `try_inline_*` refusal and the `_` arm call instead of `emit_step_and_continue` directly — counts the ones it did not lower.
  (The run-time guard fallbacks *inside* the `try_inline_*` functions keep calling `emit_step_and_continue`; they are run-time deopts, attributed by `deopts_by_variant`, not static ones.)
  The corpus is the crate's `tests/fixtures/*.beam` (discovered by directory scan, no hardcoded names) plus the installed OTP `.beam`s discovered generically from `erl`'s `code:lib_dir()` (**no hardcoded app names**; `ERTS_ORACLE` honoured; a bounded sample via `OTP_MODULE_CAP`, default 4096 covering the whole tree; the OTP part is skipped loudly without a usable `erl`).
  It compiles each function through `emit::compile_function` after `crate::jit::cache::clear_baseline_cache()`.
  Prints a **raw deopt-count** ranking (`variant | deopts | instances | deopt_ratio`) and a **weighted-score** ranking (`score = deopts * deopt_weight`, where the block-terminating dispatch opcodes `Jump`/`SelectVal`/`SelectTupleArity` weigh 4 because a deopt there also forfeits the direct block-to-block link — see `deopt_weight`'s rationale).
- **Runtime** (`../census.rs::the_runtime_deopt_census_over_the_hot_shapes_is_printed`): runs `crate::jit::bench::assert_tier_agreement()` with recording on and dumps the per-variant `Counters::deopts_by_variant` the executed blocks produced — which un-lowered opcodes dominate the **hot** shapes.
- **Dynamic per-variant execution** (same test, immediately after its deopt ranking): `crate::jit::bench::run_workloads_on(Tier::Interpreter)` with `crate::exec_census` recording on — the **executed** population itself (per `Instr` variant, plus the typed errors per variant with the first error text) over the same in-crate workload set.
  It runs on the **interpreter** tier on purpose: a native tier emits a lowered instruction as native code, so its funnel only sees the steps it deopted (exactly the population the deopt ranking already covers).
  This is the in-crate tier workload set, **not** the OTP corpus (a corpus-scale dynamic view is a `tests/` sweep's business).
  Asserts only liveness/consistency (non-empty map, both totals = their map's sum, a first-error text per erroring variant, non-empty variant keys, a ranking that faithfully re-orders the map) and writes its own `exec_census::write_dump("dynamic")` TSV, tolerating a write failure like the deopt dump.
- **Emitted byte size** (`../census.rs::the_per_kind_emitted_code_size_table_is_printed`): for the in-block lowerings already present (`Move` incl. the literal-area source, the Y-operand `test`/`select_val`/`select_tuple_arity` shapes, `Call`, the unlinked `CallLast` tail exit, the family arm, the named-BIF accessors) it compiles each shape as a single-IR-block function through `emit::compile_function` (counters on) and prints its whole-block emitted byte length against a one-`func_info`-block **deopt reference** (199 B whole block; the `2N − N` marginal isolates the 60 B per-occurrence deopt sequence).
  Per-kind attribution is exact because each shape compiles as its own single-block function; it also prints the whole-artifact `code_len` of the committed fixtures.
  `../emit/prologue.rs`'s accounting prints the Y split on the same run as `Y prologue: blocks=… publish=… elided=… invalidations=…`.

All four write a machine-readable TSV to `std::env::temp_dir()` and assert only liveness/consistency (never a threshold), so the printed rankings are the evidence and the tests pass as the inline set changes.

## Gate commands
- Static census: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_static_deopt_census_over_the_corpus_is_printed -- --nocapture`
- Emitted-byte table: the same command with `--lib the_per_kind_emitted_code_size_table_is_printed`.

## Constraints
- `#[cfg(test)]`-only: no production path reaches these modules, and no measurement may change a result.
- `counters.rs` is `#[cfg(test)]`-only, so only a unit test under `src/jit/baseline/` can reach it; `census.rs` and `emit/test/refusal_census.rs` are the only non-`tests.rs` callers.
- A warmed `CodeCache` re-emits nothing, so a counter pin must compile cache-free through `emit::compile_function` (as `../call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt` does).

## Test Strategy
The instruments are their own suites: each census test prints its table and asserts only liveness/consistency, and the structural counter pins live beside the lever they price (`../call_tests.rs`, `../tail_exit_tests.rs`, `../bif_tests.rs`, `../tests.rs::the_y_window_traffic_of_the_committed_fixtures_is_measured`, `../emit/test/refusal_census.rs`).
The enumeration is `../testing/CONTEXT.md`; the figures are `../measure/CONTEXT.md`.

## Known Issues
- **`../census.rs` is `#[cfg(test)]`-only and 1583 lines.** It is one measurement instrument with four views over one corpus-discovery helper, so splitting it is a separate, parity-neutral refactor.
- **No census pins an expected count.** Every corpus-level assertion is liveness/consistency only
  (`../census.rs:374-401`, `:507-511`, `:555-615`, `:1046-1050`, `:1327-1386` `assert_reg_cache_row_consistent`;
  `../emit/test/refusal_census.rs:419` is just `modules_loaded > 0`; `../operand_tests.rs:223-226` just
  `total > 0` + no unclassified `??`). The recorded totals therefore live **only as prose** — crate
  `../../../CONTEXT.md:184`, `../../../PERF.md:30,77`, `../../../MEASURED.md:146`, `../../../TEST_NOTES.md:83`
  (`3 113 483` instances / `119 840` deopts / `103 008` `FuncInfo`; the per-variant decomposition sums to
  `119 840` exactly) — and a rerun on a different tree will print different numbers without any test failing.
  The only exact-number assertions in this node are on **synthetic** shapes (e.g. `../counters.rs:731-760`,
  `../tests.rs:5621-5625`, `../cross_tests.rs:211-234`).
- **The static census's denominator is *visited* instances, not IR instructions.** `record_instr_instance`
  runs at the top of `emit_block`'s range loop (`../emit/block.rs:186`), but that loop `break`s at ten
  `terminated = true` sites (`:373, 477, 492, 501, 569, 640, 677, 770, 861`), so the rest of a block's
  `[start, end)` range is never counted, while `record_block` (`../counters.rs:576`, call site
  `../emit/block.rs:957-962`) records the full `end − start`. That is why the recorded
  **3 113 483** instances are smaller than the corpus's **3 585 490** IR instructions — two different
  denominators for the same corpus, never interchangeable.
- **Recorded corpus sizes drift from the tree.** The prose says "32 committed `erlc` fixtures"
  (`../../../MEASURED.md:142`, `../../../PERF.md:65`, `../../regs/CONTEXT.md:70`), but `tests/fixtures/`
  currently holds **38** `.beam` files and `../census.rs:190-203` scans the directory with no cap, so a
  rerun censuses 38. The module count is likewise recorded twice (1321 at `../../../MEASURED.md:142`,
  1353 at `../../../PERF.md:19` and `../../measure/CONTEXT.md:140`) because it follows the installed OTP
  tree. Quote these figures as "as recorded on that corpus", never as current expectations.

## See Also
- `../CONTEXT.md` → the tier contract (inline set, deopt rule, routing table).
- `../measure/CONTEXT.md` → every figure these instruments produced and the levers they chose.
- `../testing/CONTEXT.md` → the test enumeration these suites are part of.
