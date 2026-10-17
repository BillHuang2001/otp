# bench — measured results (m15)

This is the definitive summary of the **current** measurement session (`m15`, 2026-10-17, host load
≈2–5) for the `erts_rust` benchmark driver. Every number here comes from the committed raw
artifacts under `results/m15-*`; regenerate with the Runbook (§7) rather than quoting a stale copy.
Raw JSON is authoritative — this file never edits a metric value.

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys**  | the oracle `erl` on `PATH` (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt` at its **default baseline-JIT tier** (`JitMode::Auto` ⇒ `Tier::BaselineJit`) | reports `30` |

Every "sys vs rust" row below is a comparison of those two programs. **Rust-vs-Rust rows** (the
interpreter-pin tier control §5) are labelled separately and are **never** a system-ERTS comparison.

Artifacts for this session (all committed): `results/m15-bench-xrun.json` (primary,
`--full --runtime all`), `results/m15-bench-xrun-{interp,quick}.json` (tier controls,
`--quick --runtime rust`), `results/m15-bench-external.json` (`--full --runtime sys`),
`results/m15-bench-plb.json`, `results/m15-bench-emqx.json`, the diffs
`results/m15-compare-{cross-runtime,tier-effect-quick,vs-m14-xrun,vs-m14-external}.txt`, and the load
logs `results/m15-{xrun,tier,ext}-load.log` + `results/m15-load-record.md`.

## 1. The host and the load record

14 logical cores, shared and usually loaded. `/usr/bin/time` is absent; the harness uses
`date +%s%N` + `/proc/<pid>/status` `VmHWM`/`VmRSS`. Load is recorded before/after every run:

| run | artifact(s) | `/proc/loadavg` (1-min) before → after |
|---|---|---|
| primary cross-runtime (`--full`) | `m15-bench-xrun.json` | 4.91 → 2.65 |
| tier controls (`--quick`, rust-only) | `m15-bench-xrun-{interp,quick}.json` | 2.06 → 3.96 → 2.74 |
| external targets (`--full`, sys-only) | `m15-bench-external.json` | 2.68 → 2.52 |
| `plb` / `emqx` (`--quick`, sys-only) | `m15-bench-plb.json`, `m15-bench-emqx.json` | 2.40 → 2.27 (`plb`), 2.27 → 2.27 (`emqx`) |

Full logs: `m15-xrun-load.log`, `m15-tier-load.log`, `m15-ext-load.log`; table +
narrative in `m15-load-record.md`.

**m15 was a comparatively quiet session** (load ≈2–5, one process at a time) where m14 ran at
40–145. Two consequences, both of which must be stated with the numbers:

- absolute wall times and throughputs **improved from m14 to m15 simply because the host was
  quieter** — they are load-contaminated on both sides of that comparison;
- **read the ratios and the deterministic `case_*_ops/calls/elements` counters, never a single
  absolute wall time as a constant.**

`m15-xrun-load.log` also holds an **earlier, superseded** primary pass (7.31 → 5.74) that was taken
*before* the rust loop-count recalibration of §4; its numbers are used nowhere in this file.

## 2. `baseline` — startup distribution + vm memory (sys vs rust)

`bench run --full --runtime all --workloads baseline` (`m15-bench-xrun.json`).

| metric | sys | rust | note |
|---|---|---|---|
| startup `p50` (ms) | **111.6** | **282.0** | whole-process wall per `-noshell -eval 'halt().'` (n=50) |
| startup `p99` (ms) | **136.1** | **288.8** | |
| startup mean / min / max (ms) | 113.7 / 96.3 / 137.9 | 282.2 / 274.0 / 289.3 | |
| compute loop mean (ms) | 193.4 | 314.8 | `lists:sum(lists:seq(1,1e6))` |
| compute ops/s | 5.172e6 | 3.176e6 | native-floor dominated (`lists:seq/2`+`lists:sum/1`) |
| `erlang:memory(total)` (`vm_memory_total_bytes`) | 36.19 MB | **13.22 MB** | rust **2.74× lower** |
| OS peak RSS (`VmHWM`) | 135.0 MB | 133.3 MB | now nearly equal — see below |

`vm_memory_total_bytes` (the VM's own accounting) and `peak_memory_bytes` (OS `VmHWM`) are
**separate metrics and are never conflated**.

The `baseline` OS peak RSS is a whole-process figure for a workload that runs **no per-op loop**, and in m15 the
two sides are within 1.3 % of each other (135.0 vs 133.3 MB). m14 reported 136.4 vs 407.4 MB for the same
metric; that spread is **not reproduced here** and no figure on this row is affected by the §3.4 estimator
question (`baseline` takes no marginal). The **loop** workloads' rust peak RSS is still boot-dominated — see §3.2.

## 3. Per-workload-family ratios (rust ÷ sys, primary marginal)

`m15-bench-xrun.json`, `--full`, both runtimes. Ratio > 1 ⇒ rust dearer.

### 3.1 The ratios

| workload | case | sys | rust | **rust/sys** |
|---|---|---|---|---|
| `list_walk` | per element | 2.48 ns | 19.74 ns | **7.95×** |
| `copy_bound` | `etf_roundtrip` | 652.7 ns | 1 422.7 ns | **2.18×** |
| `copy_bound` | `ets_copy` | 226.2 ns | 2 536.4 ns | **11.22×** |
| `copy_bound` | `send_recv` | 1 464.3 ns | 7 696.8 ns | **5.26×** |
| `copy_bound` | `send_recv_plain` | 649.4 ns | 1 918.8 ns | **2.95×** |
| `gc_bound` | `churn` | 4 266.4 ns | 64 774.0 ns | **15.18×** |
| `foreign` | `bstr_to_lower` | 338.4 ns | 13 344.2 ns | **39.4×** |
| `foreign` | `bstr_capitalize_token` | 304.0 ns | 15 765.2 ns | **51.9×** |
| `foreign` | `emqx_base62_encode` | 200.3 ns | 17 284.1 ns | **86.3×** |
| `foreign` | `emqx_base62_decode` | 290.4 ns | 10 090.9 ns | **34.8×** |
| `slice` | 9 cases, one call/process | Σ mean wall 1 215.7 ms | Σ mean wall 2 582.6 ms | ≈**2.12×** |

The `send_recv − send_recv_plain` gap attributes the ETF-codec component *inside* the two-process
path (the two cases differ only in whether the receiver re-encodes the payload).

**All 12 primary rows are `ok`, and no row withheld a figure** (`marginal below timer resolution`
appears nowhere in the session): every rust marginal is 575–1 037 ms against a paired noise band of
±2.3–66.7 ms — a ≳8.6× margin where the publication guard requires ≳3×.

### 3.2 How to read a per-unit figure (and what is *not* like-for-like)

The per-unit figures are startup-cancelled marginals over a **per-runtime loop count `N`** (§3.4,
§4). `N` differs between the two runtimes, so the report always carries the deterministic,
contention-immune companion that names it: `case_etf_roundtrip_ops` 1 500 000 (sys) / 500 000 (rust),
`ets_copy` 2 000 000 / 250 000, `send_recv` 500 000 / 90 000, `send_recv_plain` 1 000 000 / 300 000,
`churn` 200 000 / 10 000, `list_walk_elements` 100 000 000 / 50 000 000, `foreign` `calls`
1 500 000 / 50 000 (`bstr_to_lower`), 1 500 000 / 40 000 (`bstr_capitalize_token`), 3 000 000 /
60 000 (`emqx_base62_encode`), 3 000 000 / 60 000 (`emqx_base62_decode`). **Never compare raw
`wall_ms` across two sides with different `N`** — only the per-unit normalization is comparable.

Peak memory is like-for-like **only** for `slice` (§3.3) and for `baseline`'s
`vm_memory_total_bytes`. The loop workloads' rust peak RSS is **boot-dominated** (the resident
preloaded-module set dwarfs the loop's own footprint — e.g. `copy_bound/etf_roundtrip` at 459 MB)
and is **not** a memory comparison.

### 3.3 `slice` — one call per process

`slice` runs 9 cases with `N`=1 (a fresh process per call), so its wall time and peak RSS are
whole-process, like-for-like figures: Σ of the per-case mean wall **1 215.7 ms** (sys) vs **2 582.6 ms**
(rust), ≈**2.12×**; OS peak RSS **71.7 MB** (sys) vs **120.9 MB** (rust). Every case's stdout agreed between
the runtimes (recorded in the row's `observed stdout:` note).

### 3.4 The paired estimator — why the m14 per-unit figures are invalid

The estimator that produces every `ns_per_op` / `ns_per_element` / `ns_per_call` figure was
**replaced in m15**; this subsection is the harness's correctness record.

- **What the old schedule did.** It ran **all** `N`-unit samples first and then **all** 0-unit
  samples, and formed the figure as `(mean wall@N − mean wall@0)/N`, refusing only a *negative*
  marginal. That is valid only if the process boot cost is constant over the whole measurement. On
  this shared host it is not: boot cost moves by tens — at times hundreds — of milliseconds between
  the two passes, and the mean-based difference **absorbs that drift instead of cancelling it**.
- **How it failed, concretely.** m14 published `copy_bound/etf_roundtrip` at **136.3×**; an
  independent **interleaved** measurement of the same workload in the same era gives **2.42×**, and
  m15's paired figure is **2.18×**. The inflated number was a drift artifact, not a runtime cost.
- **What the harness does now** (`src/marginal.rs`). Each round samples the `N` pass and then the
  0 pass **back to back**, so a drifting boot cost hits both members of a *pair* almost equally; the
  statistic is the **median of the paired differences**
  (`median(wall@N − wall@0)/units`, min/max recorded), which one contended round cannot move; the
  spread is `1.4826 × MAD` (a σ-equivalent, not inflated by an outlier), floored at `MIN_NOISE_MS`
  (1 ms).
- **The withholding rule.** A figure is **withheld** — no metric, plus an explicit
  `marginal below timer resolution` note — unless the median clears
  `NOISE_MULTIPLE (3) × max(MIN_NOISE_MS, 1.4826·MAD)`. A negative or non-finite marginal is never
  published either.
- **Pinned by a test.** `marginal::tests::interleaving_cancels_a_drifting_boot_cost` drives a fake
  clock that drifts by a fixed step per round and shows the paired median recovers the true per-unit
  cost exactly where the old sequential `mean(work) − mean(startup)` is off by
  `runs × drift_step` (≈6× in that fixture). The interleaving must not be reverted to the sequential
  schedule.
- **`N`-sensitivity comes with it.** On the Rust runtime the `foreign` per-call cost is still
  `N`-dependent, so the loop counts were recalibrated in m15 (§4) and a recalibrated `n_rust` is a
  **new baseline**: a per-call figure is comparable only across runs taken at the **same `N`**, and
  must always be quoted with its `case_*_calls` companion.
- **Consequence for m14 → m15 comparisons.** The `copy_bound` / `gc_bound` / `foreign` change between
  the two sessions is **dominated by the estimator fix**, not by a runtime regression or a runtime
  fix: the two sessions use a different statistic *and* different `N`, so their per-unit numbers are
  **not comparable**. Treat m14's `copy_bound`/`gc_bound`/`foreign` figures as withdrawn.

## 4. Loop-count calibration (m15)

Rule (documented in each case table): **a runtime's timed loop must do enough work that its paired
marginal stands clear of the measurement noise** — a uniform **≥0.5 s of loop work** on the rust
side. The samples are interleaved, so the residual noise is the MAD of the paired differences (a few
milliseconds), which ≥0.5 s clears by orders of magnitude. `list_walk`'s `n_rust` (50 000) and every
`n_sys` are unchanged; the other rust counts were re-derived because rust got materially faster.

| workload | case | `n_rust` m14 → m15 | loop work now |
|---|---|---|---|
| `copy_bound` | `etf_roundtrip` | 40 000 → **500 000** | ≈721 ms |
| `copy_bound` | `ets_copy` | 200 000 → **250 000** | ≈602 ms |
| `copy_bound` | `send_recv` | 10 000 → **90 000** | ≈719 ms |
| `copy_bound` | `send_recv_plain` | 15 000 → **300 000** | ≈620 ms |
| `gc_bound` | `churn` | 5 000 → **10 000** | ≈692 ms |
| `foreign` | `bstr_to_lower` | 20 000 → **50 000** | ≈670 ms |
| `foreign` | `bstr_capitalize_token` | 15 000 → **40 000** | ≈640 ms |
| `foreign` | `emqx_base62_encode` | 25 000 → **60 000** | ≈644 ms |
| `foreign` | `emqx_base62_decode` | 15 000 → **60 000** | ≈617 ms |

**Proof of resolution:** in the m15 primary pass **12/12 rows resolved** — no
`marginal below timer resolution` anywhere — with every rust marginal 575–1 037 ms against a paired
noise band of ±2.3–66.7 ms. Each file pins its own counts in
`the_rust_loop_counts_clear_the_boot_jitter_band` against the m15-measured per-unit cost.

**Maintenance rule:** when a case's measured per-unit cost changes materially, re-derive `n_rust` so
the loop work stays ≥0.5 s (and re-pin it in that test). A loop count whose marginal collapses into
the noise band yields **no** per-unit metric (an honest note), never a number.

## 5. Rust-vs-Rust: execution-tier effect (interpreter pin vs default baseline JIT)

**NOT a system-ERTS comparison** — both sides are `beam_rt`, `--runtime rust` only. Read
**like-for-like**: both controls are `--quick` (matched form, run back to back) —
`m15-bench-xrun-interp.json` at `BEAM_RT_TIER=interpreter` vs `m15-bench-xrun-quick.json` default.
`interp_over_default` > 1 ⇒ the interpreter is dearer. Raw diff:
`m15-compare-tier-effect-quick.txt`.

| case | interp ns | default JIT ns | interp/default |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 166.9 | 22.8 | **7.31×** |
| `case_churn_ns_per_op` | 163 345.0 | 64 430.7 | 2.54× |
| `case_send_recv_plain_ns_per_op` | 3 916.8 | 1 937.4 | 2.02× |
| `case_ets_copy_ns_per_op` | 3 569.1 | 2 359.0 | 1.51× |
| `case_emqx_base62_decode_ns_per_call` | 15 056.3 | 10 360.1 | 1.45× |
| `case_bstr_capitalize_token_ns_per_call` | 20 842.4 | 15 740.4 | 1.32× |
| `case_send_recv_ns_per_op` | 9 963.2 | 7 722.6 | 1.29× |
| `case_bstr_to_lower_ns_per_call` | 17 032.2 | 13 346.1 | 1.28× |
| `case_etf_roundtrip_ns_per_op` | 1 849.1 | 1 465.9 | 1.26× |
| `case_emqx_base62_encode_ns_per_call` | 21 446.5 | 17 276.2 | 1.24× |

The default baseline JIT is cheaper than the interpreter on **every** case in m15 (m14 had one
load-flipped case, `send_recv`). These are whole-process per-unit costs under each tier, from the
same paired estimator as §3; the difference is a genuine runtime-cost observation, not a harness
artifact. Both controls are `--quick`, so the run counts are 3 per case and the deterministic
`case_*_ops/calls/elements` companions still name the `N`.

## 6. External targets (all sys-only, staged in a read-only `$TMPDIR` copy)

| target | figure | instrument |
|---|---|---|
| `erlc_compiler` | **9.919 files/s**, 7 687 loc/s (60 files, wall 6 049 ms) | system `erlc` over OTP stdlib + Elixir `lib/elixir/src` |
| `elixir_compiler` | **4.709 files/s**, 2 364 loc/s (223 files, wall 47 356 ms, compiler startup 246.4 ms) | installed system `elixirc` (per-file fallback: version skew) |
| `cowboy` | **198 476 req/s**, 992 720 requests, 0 errors, latency p50 0.0743 / p99 0.7534 ms (build 43 459 ms) | `hello_world` relx release, in-process keep-alive load |
| `khepri` | **80 967 ms** wall | `rebar3 eunit` in a scratch copy |
| `plb` | **67 747 ms** wall; test time p50 544.3 / p99 3 339.2 ms; test mem 93.37 MB | dotnet tool, resolved to **dotnet 9.0.318** via `command_for_min` (rejects two 8.0.425 SDKs) |
| `emqx` | **`skipped(emqx-requires-otp-28)`** — honest skip, no number | checkout pins OTP 28.5.0.7-2; host is OTP 29.1.1 |

Details: `m15-bench-{external,plb,emqx}.json`, the diff `m15-compare-vs-m14-external.txt`. The
external rows ran at load ≈2.3–2.7, so their figures are the least load-contaminated of the session —
and correspondingly better than m14's, mostly because the host was quieter (§1).

## 7. Runbook (reproduction)

```sh
# 0. build the driver AND the runtime (from the repo root)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# 1. primary cross-runtime report
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m15-bench-xrun.json --verbose

# 2. tier controls (rust-only, BOTH --quick => matched form)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run ... run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m15-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run ... run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m15-bench-xrun-quick.json

# 3. external targets (sys-only)
erts_rust/scripts/cargo.sh run ... run --full --runtime sys \
    --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m15-bench-external.json --verbose
erts_rust/scripts/cargo.sh run ... run --quick --runtime sys --workloads plb \
    --out-file erts_rust/tools/bench/results/m15-bench-plb.json --verbose
erts_rust/scripts/cargo.sh run ... run --quick --runtime sys --workloads emqx \
    --out-file erts_rust/tools/bench/results/m15-bench-emqx.json --verbose

# 4. diffs
erts_rust/target/release/bench compare results/m15-bench-xrun.json              # cross-runtime
erts_rust/target/release/bench compare results/m15-bench-xrun-interp.json \
                                   results/m15-bench-xrun-quick.json            # matched tier diff
erts_rust/target/release/bench compare results/m14-bench-xrun.json \
                                   results/m15-bench-xrun.json                  # vs m14
```

Record `/proc/loadavg` before and after every run (`m15-*-load.log` shape), and measure **one
process at a time**. Run order is runtime-major, so a drifting load can bias a cross-runtime
comparison; for a binary-vs-binary claim use an interleaved A/B instead of two reports.

Archive `out/` copies into `results/` with **only** the two mechanical normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR`
prefix in `notes` → `$TMPDIR/`). **Never edit a metric value.** m15's artifacts were normalised
exactly that way.

## 8. Status and still-open items

- **m15 rows:** primary **12/12 `ok`**, tier controls **6/6 `ok` each (×2)**, external **4/4 `ok`**,
  `plb` **`ok`**, and the only non-`ok` row in the whole session is
  `emqx` → `skipped(emqx-requires-otp-28)`. **No row withheld a figure** — no
  `marginal below timer resolution` anywhere.
- **CLOSED — `foreign: emqx_base62:encode` vs the m10 rust binary is at parity** (0.95×–1.07×). The
  m13-era "3.2×–3.8× dearer" reading is not reproducible against the current tree; there is no open
  regression on that case. The `N`-sensitivity caveat of §3.4 still applies to *quoting* a per-call
  figure, but it is no longer an open investigation.
- **Still open — the rust-vs-sys per-unit gaps themselves** (§3.1: 2.18×–86.3×). These are real
  runtime costs, not harness artifacts: the startup-cancelled metric reproduces from a plain shell
  loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`, so CLI re-entry, per-call module
  reloads and per-call GC are ruled out. The causes are `beam_rt` measurements owned by
  `../../crates/beam_rt/PERF.md` and `CONTEXT.md`, not `bench` output.
- **Still open — the `foreign` per-call metric is `N`-dependent on rust** (§3.4): the counts were
  recalibrated in m15, so a per-call figure is comparable only at a **matched `N`**; every row emits
  its `case_*_calls = N` companion.
- **Still open — the loop workloads' rust peak RSS is boot-dominated** and is not a like-for-like
  memory comparison; `slice` (N=1) and `baseline`'s `vm_memory_total_bytes` are.
- **Still open — no self-hosted build (contract A.9).** `erlc_compiler` and `elixir_compiler` are
  `Runtimes::SysOnly` and shell out to the system `erlc`/`elixirc`; nothing in the driver runs a
  compiler under `beam_rt`.
- **Honesty caveats that travel with every figure:** the host is shared and absolute wall
  times/throughputs are load-contaminated (m15 ran at load ≈2–5, m14 at 40–145 — so m14→m15 absolute
  times also improved simply because the host was quieter); the m14→m15
  `copy_bound`/`gc_bound`/`foreign` change is dominated by the **estimator fix** (§3.4), and the two
  sessions' per-unit numbers are not comparable; `foreign: emqx_base62:encode` is `N`-sensitive;
  `baseline`'s compute loop is native-floor dominated.

**History:** prior sessions are the committed `results/m1{0,1,2,3,4}-*` artifacts (`m15-*` is
current; `m14-*` and older are regression baselines). **m14's `copy_bound`, `gc_bound` and `foreign`
per-unit figures are superseded by the estimator fix (§3.4) and must not be quoted as current.**
