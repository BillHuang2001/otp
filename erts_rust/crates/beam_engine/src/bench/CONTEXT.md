# `beam_engine::bench` — in-process foreign-workload decomposition harness

## Intent
A `#[cfg(test)]`-only harness that measures ONE real external benchmark workload **in process**, so the
engine's own per-call cost can be decomposed without the runtime's CLI boot (~2.2 s), module loading,
scheduler and copy-out glue in the timed loop.
Built to explain the `foreign` workload (`../../../../tools/bench/RESULTS.md` m15: `cowboy_bstr:to_lower/1`
at 13 344 ns/call = **39.4×** system ERTS) and to prove the in-place `private_append` change in
`../families/bitstrings/create` with deterministic counters plus an interleaved A/B.
It is absent from production builds: `../lib.rs` declares `#[cfg(test)] mod bench;`.

## API Surface
- `counting.rs` → `CountingContext`, a delegating `ExecutionContext` over `MinimalContext` that tallies
  `reduce` (= instructions executed: the engine charges it once per interpreted instruction),
  `alloc_words_rooted` calls/words (the per-built-term admission door), the
  `ensure_heap`/`ensure_heap_full` door (the `test_heap`/`allocate_heap` door), `call_bif` plus a
  `module:function/arity` histogram, `apply`/`apply_fun`, `alloc_words`, and the three resolver seams.
- `mod.rs` → the fixture (Cowboy's `to_lower/1` + `char_to_lower/1` verbatim, Apache-2.0, compiled at test
  time with the system `erlc`; the test **skips loudly** when `erlc` is absent), the batch run loop, the
  printed table, and `#[test] #[ignore] fn foreign_bstr_to_lower_decomposition`.
- Env knobs: `BENCH_ITERS` (20 000), `BENCH_BATCH` (500), `BENCH_WARMUP` (2 000),
  `BENCH_TIER` (`interpreter`|`baseline`|`both`).

## How to run
```text
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_bstr_to_lower_decomposition

# release (per-instruction cost is ~2x lower than the opt-level=1 dev profile; use release for ratios)
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib --release --no-run
taskset -c 3 erts_rust/target/release/deps/beam_engine-<hash> --exact \
    bench::foreign_bstr_to_lower_decomposition --ignored --nocapture

# profile the loop itself (the erlc child is filtered out by --comm)
perf record -g -o "$TMPDIR/p.data" -- taskset -c 3 <binary> --exact \
    bench::foreign_bstr_to_lower_decomposition --ignored --nocapture
perf report --stdio -i "$TMPDIR/p.data" --no-children --comm=bench::foreign_ \
    --percent-limit 0.8 --sort symbol
```

## The workload shape (verified with `erlc -S`)
`to_lower/1` is a **binary comprehension**, which the compiler lowers to the *same* accumulator shape a
hand-written `<<Acc/binary, (char_to_lower(C))>>` produces: `bs_init_writable` of `{integer,256}`
(32-byte capacity) plus the tail loop `'-to_lower/1-lbc$^0/2-0-'/2`, which per character runs
`bs_start_match3`, `bs_match` (`ensure_at_least 8`, `integer 8`), a **local** call to `char_to_lower/1`
(a 27-clause pure-Erlang `select_val`, no BIF), `bs_create_bin` with
`[private_append, {tr,{y,1},{t_bitstring,8,true}}, all, integer 8 {x,0}]`, and `call_last 2`.
**438 instructions and 30 characters per call.**

## Measured — the decomposition (release, `taskset -c 3`, shared host)
15 interleaved samples per side, alternating binary order round by round; `/proc/loadavg`
4.76/4.42/4.59 before the sweep, 3.49/4.14/4.49 after. Absolute wall times are not signal on a shared
host; the ratios and the counters are.

| counter (per call) | before in-place `private_append` | after |
|---|---|---|
| `instr_per_call` (interpreter `reduce` count) | 438 | **438** — unchanged, no semantic change |
| `alloc_words_rooted` calls | 62 | **32** |
| `alloc_words_rooted` words | 295 | **163** |
| off-heap refc payloads created | 1 | 1 (the copy-rebuild built *on-heap* `ErlHeapBits`, not refc) |
| `ensure_heap`/`ensure_heap_full` calls | 0 | 0 |
| `call_bif` calls | 0 | 0 |
| local-call seam requests | 61 | 61 |
| min / median ns per call, **interpreter** | 17 592 / 17 805 | **13 387 / 13 672** → **1.31×** |
| min / median ns per call, **baseline JIT (default tier)** | 12 726 / 12 812 | **7 119 / 7 204** → **1.79×** |

What the difference is: **30 of 62 rooted reservations** (one per character), the bit-at-a-time copy of
the accumulated binary, and the growable `Vec` it was assembled in. `perf` self time says the same thing
directly — `BitWriter::push_bits_from` was **27.4 %** of the before binary's samples and is below the
0.8 % cut-off in the after binary.

What is left is the dispatch floor plus one allocation: 438 instructions at ≈16.8 ns each on the baseline
tier (≈31 ns each on the interpreter), a static deopt census of **zero** useful deopts (only the deliberate
never-executed `FuncInfo`), `jit_step_one`/`jit_step_ext`/`driver_interp_steps` all 0, 63 driver
round-trips per call, and `<CarrierAllocator>::alloc_impl` (the single `bs_init_writable` payload) as the
top remaining frame at **17.3 %**. The next lever on this shape is therefore `bs_init_writable`'s payload
allocation — not dispatch, and not the append path.

## Constraints / Notes for Agents
- **`MinimalContext` opts out of the reduction hook** (`wants_reduce() == false`), so `CountingContext`
  MUST override it to `true` or the instruction count silently stays 0.
- **No collector here.** `MinimalContext` only grows its reserved heap, so this harness measures the
  engine, never GC cadence — and for this shape a collection-free figure is faithful: the runtime side
  records that `bstr_to_lower` reaches **no** heap-admission point at all and reports `collections: 0`
  (it allocates only through `alloc_words_rooted`); see `../../../../crates/beam_rt/src/runner/CONTEXT.md`
  §Measured and `../../../../crates/beam_rt/M1_GAPS.md`.
- **Do not re-investigate the older `foreign` regression.** The Θ(N³) cost (the runner re-registering the
  whole never-cleared `refc_binaries` reservoir onto the process once per reduction slice) is fixed
  (`07a5720639` + `7d84b12b44`) and re-verified; `../../../../tools/bench/RESULTS.md` §8 withdraws the
  pre-m15 `foreign` figures (different `N` and an estimator fix).
- **Batch lifecycle is load-bearing.** `Machine::refc_binaries` and the `MinimalContext` region heap grow
  monotonically and are never freed, so every batch builds a fresh `Machine` + context (default 500 calls)
  and drops both at batch end.
- The **census pass reloads the module** (a second `load_file`): the per-thread compiled-block cache would
  otherwise answer the pass from an artifact compiled while recording was off, leaving `instr_instances`
  empty.
- The `#[ignore]`d test asserts only cheap invariants — correct bytes on both tiers, the tiers agreeing,
  0 BIF dispatch — and **no timing**, so it never fails on a loaded host.

## Routing Table
- `mod.rs` → the fixture (the `erlc` compile + skip), the batch run loop, the printed table, the
  `#[ignore]`d entry point, `baseline_census` (the per-thread `../jit/baseline/counters.rs` snapshot pass).
- `counting.rs` → `CountingContext` (the delegating counting `ExecutionContext`).
