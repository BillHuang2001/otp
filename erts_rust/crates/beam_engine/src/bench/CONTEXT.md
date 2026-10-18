# `beam_engine::bench` — in-process foreign-workload decomposition harness

## Intent
A `#[cfg(test)]`-only harness that measures real external benchmark workloads **in process**, so the
engine's own per-call cost can be decomposed without the runtime's CLI boot (~2.2 s), module loading,
scheduler and copy-out glue in the timed loop.
Two workloads, one methodology:
1. `bstr_to_lower` — Cowboy's `cowboy_bstr:to_lower/1`, which explains the `foreign` workload in
   `../../../../tools/bench/RESULTS.md` (m15: 13 344 ns/call = **39.4×** system ERTS). Its accumulator
   **fits** its hint, so it measures the plain build + dispatch floor.
2. `long_append` — a binary accumulator that genuinely **outgrows** its `bs_init_writable` hint, so it
   measures the writable payload's **growth** path (`BitWriter::ensure_room` →
   `Machine::grow_writable_payload` → `beam_alloc`'s `RefcBinary::ensure_capacity`).
It is absent from production builds: `../lib.rs` declares `#[cfg(test)] mod bench;`.

## API Surface
- `counting.rs` → `CountingContext`, a delegating `ExecutionContext` over `MinimalContext` that tallies
  `reduce` (= instructions executed: the engine charges it once per interpreted instruction),
  `alloc_words_rooted` calls/words (the per-built-term admission door), the
  `ensure_heap`/`ensure_heap_full` door (the `test_heap`/`allocate_heap` door), `call_bif` plus a
  `module:function/arity` histogram, `apply`/`apply_fun`, `alloc_words`, and the three resolver seams.
- `mod.rs` → the **shared harness**: `compile_fixture(module, source, tag)` (fixture compiled at test
  time with the system `erlc`; every workload **skips loudly** when `erlc` is absent), the `Prepare`
  closure that builds a batch's argument, `run_calls` (the batch loop), `measure`, `Measurement`
  (`min`/`median`), `ns_per_call`, the env knobs, and the `bstr_to_lower` fixture + its report
  (`foreign_bstr_to_lower_decomposition`, including `baseline_census`).
- `long_append.rs` → the long-append fixture + `foreign_long_append_decomposition` + its report,
  including the growth-shape probe over `Machine::refc_binaries()`.
- Env knobs: `BENCH_ITERS`, `BENCH_BATCH`, `BENCH_WARMUP`, `BENCH_TIER` (`interpreter`|`baseline`|`both`);
  defaults are per workload (`bstr_to_lower`: 20 000 / 500 / 2 000; `long_append`: 50 / 5 / 5, because a
  long-append call copies 64 KiB out).

## How to run
```text
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_bstr_to_lower_decomposition
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_long_append_decomposition

# release (per-instruction cost is ~2x lower than the opt-level=1 dev profile; use release for ratios)
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib --release --no-run
taskset -c 3 erts_rust/target/release/deps/beam_engine-<hash> --exact \
    bench::long_append::foreign_long_append_decomposition --ignored --nocapture

# profile a loop itself (the erlc child is filtered out by --comm)
perf record -g -o "$TMPDIR/p.data" -- taskset -c 3 <binary> --exact <test path> --ignored --nocapture
perf report --stdio -i "$TMPDIR/p.data" --no-children --comm=bench::foreign_ \
    --percent-limit 0.8 --sort symbol
```

## Workload 1 — `cowboy_bstr:to_lower/1` (verified with `erlc -S`)
`to_lower/1` is a **binary comprehension**, which the compiler lowers to the *same* accumulator shape a
hand-written `<<Acc/binary, (char_to_lower(C))>>` produces: `bs_init_writable` of `{integer,256}`
(32-byte capacity) plus the tail loop `'-to_lower/1-lbc$^0/2-0-'/2`, which per character runs
`bs_start_match3`, `bs_match` (`ensure_at_least 8`, `integer 8`), a **local** call to `char_to_lower/1`
(a 27-clause pure-Erlang `select_val`, no BIF), `bs_create_bin` with
`[private_append, {tr,{y,1},{t_bitstring,8,true}}, all, integer 8 {x,0}]`, and `call_last 2`.
**438 instructions and 30 characters per call**, so the 30-byte result fits the 32-byte hint on every
call: this workload never grows the payload.

## Workload 2 — `long_append:expand/1` (verified with `erlc -S`)
```
input(N)  -> <<16#00112233445566778899aabbccddeeff:N/unit:8,
               16#ffeeddccbbaa99887766554433221100:N/unit:8>>.
expand(B) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= B >>.
```
- **BIF-free by construction** (the inner host rejects every BIF): `input/1` is one `bs_create_bin` of
  two dynamic-size `integer` segments (an `integer` segment of `N * unit` bits encodes the value
  zero-extended, so each half is `N` bytes with a distinct 16-byte marker near its end), and `expand/1`
  appends four `integer 8` segments per input byte.
- `erlc -S` on `expand/1` gives `{move,{integer,256},{x,0}}. bs_init_writable.` (a **256-BIT = 32-byte**
  hint) and, in the comprehension body `'-expand/1-lbc$^0/2-0-'/2`, `bs_start_match3` + `bs_match`
  (`ensure_at_least 8`, `integer 8`) then `bs_create_bin` whose **first** segment is
  `{atom,private_append}, {tr,{x,1},{t_bitstring,32,true}}, {atom,all}` followed by four `integer 8`
  segments and a tail `call_only 2`. That is exactly C's writable-accumulator idiom
  (`erl_bits.c:1965-2020`).
- The hint is **not** an estimate of the result: the argument's size is unknown to `expand/1`, so the
  compiler falls back to its 256-bit default. Building the input *inside* `expand/1` would make the hint
  `bit_size(Bin) * 4` — exact — and nothing would grow; that is why `input/1` is a separate function
  (and why a **list** comprehension is the wrong shape: the compiler emits a `length/1`-derived hint).
- Sized to `N_ARG = 8 * 1024` → **16 384 bytes in, 65 536 bytes out (64 KiB), 16 384 appends, 2048× the
  hint**. `input/1` runs once per batch as *setup*: the counters are parked across it and the payload
  roster is snapshotted after it, so neither its instructions nor its payload land in the tally.

### Measured — the long-append decomposition (interpreter tier, shared host)
The counters below are deterministic (host-side tallies); the wall clock is the **dev profile**
(`opt-level = 1`), which is what the `--ignored` command above produces.| counter (per call) | value |
|---|---|
| `instr_per_call` (interpreter `reduce` count) | 98 318 (6 per appended element) |
| `rooted_calls_per_call` / `words` | 16 386 / 81 933 — the 5-word `bs_start_match3` match context per iteration, **not** the accumulator; identical on the growth and the copy-rebuild route |
| `accumulator_payloads_per_call` | **1** (one writable payload per call, however many appends) |
| `heap_door_calls_per_call` | 0 (the fixture has no `test_heap`/`allocate_heap`) |
| `bif_calls_per_call` | 0 (asserted) |
| `local_call_requests_per_call` | 16 385 (the comprehension's tail `call_only`) |

The accumulator payload after one call (`Machine::refc_binaries()`, printed by the growth-shape probe):

| `len` | `capacity` | `reserved` | `in_place_grows` | `moves` | `bytes_copied` |
|---|---|---|---|---|---|
| 65 536 B | 65 536 B | 65 536 B | 2 | 6 | **64 512 B** |

`bytes_copied` ≈ `n` and `moves` = 6 (O(log n)) **is** the amortisation proof, and it is structural —
no wall clock involved. The copy-rebuild form this path replaced (`≈ n² / (2 · 4)` = **536 903 680 B**
for this build, `copy_rebuild_bytes_counterfactual` in the printed block) would have copied ~**8 300×**
more.

### Measured — A/B against the pre-adoption tree (`b52f403a30`)
Both sides built `--release`; 3 rounds per side, alternating order round by round, `taskset -c 3`,
`BENCH_ITERS=25 BENCH_BATCH=5 BENCH_WARMUP=5 BENCH_TIER=both` (so 15 sub-run samples per side per tier).
`/proc/loadavg` 4.98/13.60/23.41 before the sweep, 9.12/12.93/22.50 after — **the host is shared, so
read the ratio, not the absolute times.** before = `b52f403a30` copy-rebuild, after = HEAD grown in
place.

| side | interpreter min / median ns per call | baseline-JIT min / median ns per call |
|---|---|---|
| before (`b52f403a30`, copy-rebuild) | 307 444 860 / 308 527 146 | 306 418 480 / 307 627 433 |
| after (HEAD, grown in place) | **3 505 114 / 3 520 832** | **2 885 965 / 2 919 774** |
| ratio | **87.7× / 87.6×** | **106.2× / 105.4×** |

(best-of-3-rounds of the per-round 5-sub-run minimum/median; the third after-round ran at load 9.1 and
is 1.4× slower than the first two, which is exactly why the minimum is the reported statistic.)

All the deterministic counters are identical on the two sides **except** the payload roster, which is the
whole story: `accumulator_payloads_per_call` **16 321 → 1**, and the returned payload's growth counters
**`in_place_grows`/`moves`/`bytes_copied` 0/0/0 → 2/6/64 512**. `instr_per_call` is 98 318 on both sides
(no semantic change), `bif_calls_per_call` 0 on both, `rooted_calls_per_call` 16 386 on both (the match
context), and `result_is_correct = true` on both.

## Workload 1 — the measured decomposition (release, `taskset -c 3`, shared host)
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

A **second**, independent interleaved sweep of the two binaries at the change's final tree (15 samples
per side, alternating, `taskset -c 3`, `/proc/loadavg` 14.57/7.52/5.51 → 10.30/7.15/5.45) reproduces the
same ratios on a *busier* host: interpreter 17 776 / **18 258** → 13 293 / **13 617** ns per call
(**1.34×**), baseline tier 12 771 / **13 277** → 7 233 / **7 382** ns per call (**1.80×**), with every
counter identical (438 instructions, 62 → 32 rooted calls, 295 → 163 words).

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
- **A batch's argument is setup, never measurement.** `run_calls` parks the whole `Counters` set across
  the `Prepare` call and snapshots `Machine::refc_binaries().len()` after it, so a fixture that builds
  its own input *through the engine* (as `long_append` must — `make_heap_binary` refuses > 64 bytes)
  contributes neither instructions nor payloads to the tally. Do not move fixture setup inside the timed
  loop.
- **No collector here.** `MinimalContext` only grows its reserved heap, so this harness measures the
  engine, never GC cadence — and for both shapes a collection-free figure is faithful: the runtime side
  records that `bstr_to_lower` reaches **no** heap-admission point at all and reports `collections: 0`
  (it allocates only through `alloc_words_rooted`); `long_append` likewise reports
  `heap_door_calls_per_call = 0`; see `../../../../crates/beam_rt/src/runner/CONTEXT.md`
  §Measured and `../../../../crates/beam_rt/M1_GAPS.md`.
- **Do not re-investigate the older `foreign` regression.** The Θ(N³) cost (the runner re-registering the
  whole never-cleared `refc_binaries` reservoir onto the process once per reduction slice) is fixed
  (`07a5720639` + `7d84b12b44`) and re-verified; `../../../../tools/bench/RESULTS.md` §8 withdraws the
  pre-m15 `foreign` figures (different `N` and an estimator fix).
- **Batch lifecycle is load-bearing.** `Machine::refc_binaries` and the `MinimalContext` region heap grow
  monotonically and are never freed, so every batch builds a fresh `Machine` + context (default 500 calls
  for `bstr_to_lower`, 5 for `long_append`) and drops both at batch end.
- The **census pass reloads the module** (a second `load_file`): the per-thread compiled-block cache would
  otherwise answer the pass from an artifact compiled while recording was off, leaving `instr_instances`
  empty.
- Both `#[ignore]`d tests assert only cheap invariants — correct bytes on both tiers, the tiers agreeing,
  0 BIF dispatch, and (long-append, only on a growing tree) `bytes_copied < 8 × n` — and **no timing**, so
  they never fail on a loaded host.
- **The growth-shape probe is tree-aware.** It prints rather than asserts the payload count: on a
  pre-growth tree `payloads_created_by_the_call` is one per *append* and the returned payload's growth
  counters are all zero (it was freshly built), which is the structural signature of the copy-rebuild
  route. That is why the same file runs against `b52f403a30` for the A/B.

## Routing Table
- `mod.rs` → the shared harness (`compile_fixture`, `Prepare`, `run_calls`, `measure`, `Measurement`,
  the env knobs) + the `cowboy_bstr` fixture, its `#[ignore]`d entry point and `baseline_census` (the
  per-thread `../jit/baseline/counters.rs` snapshot pass).
- `counting.rs` → `CountingContext` (the delegating counting `ExecutionContext`).
- `long_append.rs` → the growing-accumulator fixture, its `#[ignore]`d entry point, its report and the
  growth-shape probe (`Machine::refc_binaries()` → `capacity`/`reserved_bytes`/`growth_counters`).
