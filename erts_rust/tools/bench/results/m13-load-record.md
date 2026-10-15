# m13 measurement pass — load record & results pointer

Measurement-and-archive record for the **m13** pass: the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, the two `--quick` tier controls, the sys-only
external-target run, the matched-`N` three-way A/B (HEAD vs m12 vs m10), the CLI latency probe and the
`foreign` N-ladder.
Facts only: the exact commands, the `/proc/loadavg` observed in each run's window, the commit measured,
and the per-batch status. The numbers themselves live in the committed artifacts beside this file:

- `m13-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`, **12/12 rows
  `ok`**). This is the report the cross-runtime ratios below are read from.
- `m13-bench-xrun-interp.json` — the same six workloads, `--runtime rust` only, with
  `BEAM_RT_TIER=interpreter` pinned (the **Rust-vs-Rust tier-effect** control — it contains **no** `sys`
  rows and is NOT a system-ERTS comparison). Profile `quick`.
- `m13-bench-xrun-quick.json` — the **matched-form** control: the identical six workloads, `--runtime
  rust`, the **default** (baseline-JIT) tier, profile `quick` — the *same form* as the interpreter
  control, so the tier table is read off two `--quick` runs.
- `m13-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**).
- `m13-bench-plb.json` — `ok` (the version-aware tool resolution picks `dotnet` 9.0.318).
- `m13-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.
- `m13-ab-{head,m12,m10}-r{1,2,3}.json` — the matched-`N` three-way A/B (nine reports, three interleaved
  rounds; **round 3 is excluded** — see §9).
- `m13-cli-probe.csv` — the CLI latency probe (interleaved 5 reps × 3 probes × {head, m12, m10, sys}).
- `m13-foreign-ladder.csv` — the `foreign` N-ladder (interleaved 5 passes × {head, m12, m10, sys} ×
  {enc, cap, dec, lower} × N = 0/5k/10k/25k/60k, 400 rows).
- `m13-xrun-load.log` / `m13-tier-load.log` / `m13-ext-load.log` — the 1-min `/proc/loadavg` samples
  taken while each batch ran (15 s interval).
- `m13-compare-cross-runtime.txt` — `bench compare` for the cross-runtime report (sys vs rust inside one
  file).
- `m13-compare-tier-effect-quick.txt` — the **matched-form** `--quick` ↔ `--quick` tier control (§6).
- `m13-compare-tier-effect.txt` — `bench compare m13-bench-xrun-interp.json m13-bench-xrun.json` (Rust
  interpreter `--quick` → Rust default `--full`; **form mismatch**).
- `m13-compare-vs-m12-xrun.txt` / `m13-compare-vs-m12-external.txt` — `bench compare` against the m12
  record (**record-vs-record, measured under very different host load** — see §8).

> **Commit measured.** The driver **and** `beam_rt` were both built from HEAD **`a141d6878c`**
> (`--release -p beam_rt -p bench`) before the pass; the m13 measurement artifacts were committed as
> **`7801fabaea`**. The A/B's `m12` binary is `beam_rt` from **`ee3e426db0`** and its `m10` binary is
> `beam_rt` from **`b4593818b3`**. A sibling agent may rebuild `beam_rt`/`beam_engine` concurrently, so
> re-record the commit if the figures are re-run.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed 14-core
> machine and **in the m13 session it was never quiet**: the 1-min `/proc/loadavg` ran in the
> **≈12–136** band across the session, driven by **sibling agents outside this task**. There was **no
> genuinely quiet window except the CLI probe's** (§9; 1-min load ≈20–24). The only report whose window
> even approached the m12 quiet re-take was the tier controls' (1-min load 59.30–87.12).
> **Consequences for reading anything here:**
> - Every wall/latency/throughput absolute in `m13-bench-xrun.json` and `m13-bench-external.json` is
>   **inflated by CPU contention**, on **both** sides.
> - The runner is **runtime-major** (all `sys` rows first, then all `rust` rows), so a *drifting* load
>   biases a cross-runtime ratio; the xrun window started at `90.68`, peaked at `135.80` and closed at
>   `81.55` (min `72.46`) — i.e. it drifted, so part of the m13-vs-m12 movement is window placement, not
>   code.
> - The two m13 tier controls **are** `--quick` and **matched-form**, but they were taken ~2 minutes
>   apart on a moving host, so their diffs are load-sensitive.
> - The load-robust signals remain the **per-op / per-element / per-call marginals**
>   (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counts** (`*_ops`,
>   `*_elements`, `*_calls`), because the per-runtime loop count `N` is fixed per case — plus the
>   matched-`N` **driver A/B** (§9), which is the intended estimator for a HEAD-vs-m12-vs-m10
>   comparison.

## 1. Environment

- Commit measured: `a141d6878c` (runtime+driver tree); artifacts committed as `7801fabaea`.
- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in every `Report`.
- Host: NixOS 26.11 (Zokor), Linux 7.2.9 (`x86_64`), Intel(R) Core(TM) Ultra 7 356H, `nproc` = 14,
  `MemTotal` = 65 317 432 kB (~62.3 GiB).
- `erlc` = the oracle's `erlc` (the cases' notes name
  `…/erts-17.1/bin/erlc`); `elixirc`/`mix` **1.20.4** ("compiled with Erlang/OTP 29").
- `dotnet` **9.0.318** is on `PATH` (`/run/current-system/sw/bin/dotnet`); the store also holds two
  8.0.425 SDKs, which the version-aware resolver rejects for `plb` (min `9`).
- `rebar3` is **not** on `PATH` — resolved from the Nix store by the driver's store scan
  (`rebar3-3.27.1`), so `khepri` runs with no `PATH` prefix.
- `$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are **unset**; `$ERTS_ORACLE` / `$BEAM_RT` / `$BEAM_RT_TIER`
  are **unset** for every run except the interpreter control (which exports `BEAM_RT_TIER=interpreter`)
  and the A/B (which exports `BEAM_RT` per variant).
- `$HOME` is read-only; `/usr/bin/time` is **absent** (the driver's own timers and `/proc/<pid>`
  `VmHWM` polling are used).

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# A. matched-N three-way A/B — identical argv, identical case-table N on all three sides; only the
#    runtime binary changes ($BEAM_RT overridden to each variant's own beam_rt). Three interleaved
#    rounds, each round run head -> m12 -> m10. round 3 is EXCLUDED (§9).
BEAM_RT=<head-worktree>/erts_rust/target/release/beam_rt                     \
    erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,copy_bound,foreign \
    --out-file erts_rust/tools/bench/results/m13-ab-head-r1.json
BEAM_RT=$TMPDIR/m12wt/erts_rust/target/release/beam_rt                                                       \
    erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,copy_bound,foreign \
    --out-file erts_rust/tools/bench/results/m13-ab-m12-r1.json
BEAM_RT=$TMPDIR/m10wt/erts_rust/target/release/beam_rt                                                       \
    erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,copy_bound,foreign \
    --out-file erts_rust/tools/bench/results/m13-ab-m10-r1.json
# … the same three commands repeated as -r2 and -r3

# B. FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun.json --verbose

# C. tier controls — same six workloads, rust only
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun-quick.json

# D. FULL sys-only external targets (one report)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m13-bench-external.json --verbose

# E. PLB (quick)   F. EMQX (quick, skips immediately)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m13-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m13-bench-emqx.json --verbose

# G. `foreign` N-ladder — direct beam_rt run on bench_foreign_driver:{enc,cap,dec,lower}, interleaved
#    5 passes x {head,m12,m10,sys} x N=0/5000/10000/25000/60000, startup-cancelled marginal, per-row
#    loadavg -> results/m13-foreign-ladder.csv   (§9, corroboration only)

# H. CLI probe — interleaved 5 reps x 3 probes x {head,m12,m10,sys}; the `-boot bootstrap/bin/start`
#    probe is run with cwd = each variant's own worktree root, so the relative boot path resolves
#    (the `sys` variant has no boot row: the system `erl` has no such in-tree script) -> results/m13-cli-probe.csv

# I. compare artifacts (the tool diffs the mean)
erts_rust/target/release/bench compare .../m13-bench-xrun.json > .../m13-compare-cross-runtime.txt
erts_rust/target/release/bench compare .../m13-bench-xrun-interp.json .../m13-bench-xrun-quick.json > .../m13-compare-tier-effect-quick.txt
erts_rust/target/release/bench compare .../m13-bench-xrun-interp.json .../m13-bench-xrun.json       > .../m13-compare-tier-effect.txt
erts_rust/target/release/bench compare .../m12-bench-xrun.json     .../m13-bench-xrun.json         > .../m13-compare-vs-m12-xrun.txt
erts_rust/target/release/bench compare .../m12-bench-external.json .../m13-bench-external.json     > .../m13-compare-vs-m12-external.txt
```

Workloads were run **serially** — this driver was the only benchmark process, one run at a time. The
artifacts were written to `results/` with only the two mechanical path normalisations (a run-host
worktree prefix in `runtimes[].program` → `erts_rust/target/release/beam_rt`, and the `m12`/`m10` A/B
binary paths → `$TMPDIR/m12wt/…` / `$TMPDIR/m10wt/…`; a scratch `$TMPDIR` prefix in `notes` →
`$TMPDIR/`). **No metric value was altered.**

## 3. `/proc/loadavg` record (1-min value first) — one row per batch, in the order the batches ran

| # | batch | command | window (UTC, observed) | 1-min load in window | artifact(s) |
|---|---|---|---|---|---|
| 4 | **matched-N A/B** (head / m12 / m10, 3 rounds) | `run --quick --runtime rust --workloads baseline,copy_bound,foreign` with `$BEAM_RT` per variant | artifact completion window `20:20:28 → 20:42:10` (**no load log covers this window**) | not logged; round 3 hit a load spike ≈`124` (§9) | `m13-ab-{head,m12,m10}-r{1,2,3}.json` |
| 1 | **FULL cross-runtime pass** | `run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound` | `20:44:43 → 20:58:13` (report generated `20:58:20`) | **min `72.46` / median `113.13` / max `135.80`** (55 samples @15 s) | `m13-bench-xrun.json` |
| 2 | **tier controls** (interpreter then matched default, both `--quick`) | `run --quick --runtime rust --workloads <same six>` | `20:58:34 → 21:02:49` (reports generated `21:00:42`, `21:02:56`) | **min `59.30` / median `73.08` / max `87.12`** (18 samples) | `m13-bench-xrun-interp.json`, `m13-bench-xrun-quick.json` |
| 3 | **sys-only external targets** (`--full`) + `plb` + `emqx` (`--quick`) | `run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri`; then `… --workloads plb`; then `… --workloads emqx` | `21:03:00 → 21:09:30` (reports generated `21:06:59`, `21:09:36`, `21:09:37`) | **min `69.46` / median `81.97` / max `99.87`** (27 samples) | `m13-bench-external.json`, `m13-bench-plb.json`, `m13-bench-emqx.json` |
| 6 | **`foreign` N-ladder** | direct `beam_rt` runs, 5 interleaved passes × 4 variants × 4 probes × N∈{0,5k,10k,25k,60k} | no per-row timestamps; bracketed by batch 3's end (`21:09:30`, load `88.25`) and batch 5's start (`21:15:18`, load `23.63`) — its own last row reads `23.29` | per-row loadavg in the CSV: **min `15.25` / max `91.01`** (first row `89.53`, last row `23.29`; 400 rows) | `m13-foreign-ladder.csv` |
| 5 | **CLI probe** | interleaved 5 reps × 3 probes × {head, m12, m10, sys} (see §9) | `21:15:18 → 21:15:53` | **`20.11`–`23.63`** (55 rows; the session's only quiet window) | `m13-cli-probe.csv` |
| — | **`compare` diffs** (no measurement) | `bench compare <a> <b>` (§2/I) | — | — | `m13-compare-cross-runtime.txt`, `m13-compare-tier-effect-quick.txt`, `m13-compare-tier-effect.txt`, `m13-compare-vs-m12-xrun.txt`, `m13-compare-vs-m12-external.txt` |

The archived reports' own `generated_at_unix_ms` (UTC) are: the A/B nine `20:20:28 → 20:42:10`
(head-r1 → m10-r3), `m13-bench-xrun.json` **20:58:20**, `m13-bench-xrun-interp.json` **21:00:42**,
`m13-bench-xrun-quick.json` **21:02:56**, `m13-bench-external.json` **21:06:59**,
`m13-bench-plb.json` **21:09:36**, `m13-bench-emqx.json` **21:09:37**, then the CLI probe
`21:15:18 → 21:15:53`. Session span (first A/B report → last CLI-probe row): **2026-10-15 20:20–21:15
UTC** (= 2026-10-16 04:20–05:15 +0800). The 1-min load across the session ranged **≈12 → ≈136**.

### Discarded and abandoned re-takes (no artifact kept)

- An **initial full-pass re-take was discarded**: the 1-min load **spiked from `18.67` to ≈`102`** during
  it, so (exactly as with m12's `m12-bench-xrun-contended.json`, except that this one is **not** kept)
  its `sys` rows and its `rust` rows would have been measured in different conditions. No artifact from
  it survives.
- A **later, quieter re-take was attempted and abandoned** when the load window closed again — the load
  **fell to ≈`12`** and then **rose to ≈`84`** before the run could finish, so it too produced **no**
  artifact.
- The committed `m13-bench-xrun.json` is therefore the pass that actually completed, taken in the
  drifting `72.46 → 135.80` window recorded above — **not** a quiet-host pass. That is why §8 labels the
  m13-vs-m12 numbers a *record-vs-record* comparison.

## 4. Per-workload status (full cross-runtime pass, `m13-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust | cases |
|---|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` | — |
| `slice`      | compute   | `ok` | `ok` | 9/9 ok on each side |
| `foreign`    | compute   | `ok` | `ok` | 4/4 ok on each side |
| `list_walk`  | compute   | `ok` | `ok` | 1/1 ok on each side (rust emits no `ns_per_element` — see §5.1) |
| `copy_bound` | messaging | `ok` | `ok` | 4/4 ok on each side (rust emits no `etf_roundtrip` `ns_per_op` — see §5.1) |
| `gc_bound`   | gc        | `ok` | `ok` | 1/1 ok on each side |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics. Every copy/churn/walk
case validated its own round-trip with a bound-variable pattern on both sides and printed the
deterministic constant (recorded in the report's `observed stdout:` notes), so the two runtimes executed
identical work. Note that m12's one rust casualty — `copy_bound`'s `send_recv_plain`, which hit the 60 s
`RUN_DEADLINE` — **produced a metric at m13** (§9).

The tier controls are **6 rust rows each, all `ok`** (no `sys` rows by construction). The full external
pass (`m13-bench-external.json`) is **4 rows, all `ok`**. `m13-bench-plb.json` is **1 `ok`** row;
`m13-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`rust/sys` > 1 means the Rust side costs **more** per unit (its ns value is higher). These are read from
`m13-bench-xrun.json`, which ran in the **drifting `72.46 → 135.80`** window (§3) — absolute values are
load-inflated on both sides. **Both sides of every ratio are named** — `sys` is the system ERTS
(OTP 29.1.1 / erts-17.1), `rust` is `beam_rt` at its **default** (baseline-JIT) tier.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 315.3467 ms | 1 061.4786 ms | **3.37×** (rust dearer) |
| `baseline`   | `vm_memory_total_bytes` | 36 197 840 B | 15 889 338 B | **0.439×** (rust 2.278× lower) |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 247.61–465.79 ms | 856.63–1 082.15 ms | **1.96×–3.85×** (rust dearer) |
| `foreign`    | `case_<name>_ns_per_call` | 324.45–548.45 ns | 21 624.28–74 352.55 ns | **49.8×–229.2×** (rust dearer) |
| `list_walk`  | `case_list_walk_ns_per_element` | 7.1059 ns | **absent** (`marginal below timer resolution`) | — (no matched pair) |
| `copy_bound` | `case_<name>_ns_per_op` | 340.09–1 787.09 ns | 15 470.90–217 031.88 ns | **45.5×–234.5×** (rust dearer; `etf_roundtrip` absent on rust) |
| `gc_bound`   | `case_churn_ns_per_op` | 7 967.6115 ns | 330 493.5681 ns | **41.5×** (rust dearer) |

### 5.1 Per-case detail

`baseline` (whole-process cost):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean, n=50) | 315.3467 | 1 061.4786 | 3.37× |
| `startup_ms` (p50 / p99) | 293.2932 / 532.0101 | 1 024.5239 / 1 624.8898 | — |
| `compute_loop_ms` (mean, n=5) | 377.1932 | 1 325.9140 | 3.52× |
| `compute_ops_per_sec` | 2 651 161.11 | 754 196.71 | rust 0.284× |
| `vm_memory_total_bytes` | 36 197 840 | 15 889 338 | rust 2.278× lower |
| `peak_memory_bytes` (RSS) | 137 564 160 (131.19 MiB) | 408 158 208 (389.25 MiB) | rust 2.97× higher |

`slice` (one call per process: `case_<name>_wall_ms` mean; peak RSS p50):

| case | sys wall_ms | rust wall_ms | rust/sys | sys RSS (MiB) | rust RSS (MiB) |
|---|---|---|---|---|---|
| `add` | 281.2829 | 1 082.1479 | 3.85× | 66.37 | 376.23 |
| `fib` | 247.6073 | 947.1394 | 3.83× | 66.32 | 376.22 |
| `pick` | 294.4713 | 1 051.5465 | 3.57× | 66.46 | 376.22 |
| `calendar_last_day` | 278.8454 | 905.9719 | 3.25× | 66.37 | 376.24 |
| `queue_from_size` | 307.4515 | 926.2598 | 3.01× | 66.80 | 376.28 |
| `gb_trees_size` | 465.7879 | 1 059.4118 | 2.27× | 66.73 | 376.22 |
| `base64_encode_size` | 462.9703 | 905.8004 | 1.96× | 66.86 | 377.27 |
| `ordsets_from_size` | 404.9216 | 856.6336 | 2.12× | 66.36 | 376.23 |
| `string_str_length` | 363.3154 | 1 078.0630 | 2.97× | 66.67 | 376.69 |

`slice` whole-workload `peak_memory_bytes`: sys 72 790 016 (69.42 MiB) → rust 395 657 216 (377.33 MiB).
**The rust RSS is the booted runtime's resident preloaded-module set, not the loop's own footprint.**

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys | sys startup p50 (ms) | rust startup p50 (ms) | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `bstr_to_lower` | 523.8590 | 44 293.3693 | 84.55× | 425.17 | 806.50 | 66.88 | 438.53 |
| `bstr_capitalize_token` | 548.4547 | 33 094.2801 | 60.34× | 424.06 | 865.29 | 66.45 | 441.24 |
| `emqx_base62_encode` | 324.4488 | 74 352.5495 | 229.17× | 374.21 | 761.73 | 66.40 | 427.78 |
| `emqx_base62_decode` | 434.0741 | 21 624.2808 | 49.82× | 379.90 | 845.36 | 67.01 | 427.90 |

`foreign` whole-workload `peak_memory_bytes`: sys 72 310 784 (68.96 MiB) → rust 462 860 288 (441.42 MiB).
The rust row carries the driver's standing caveat: **this per-call metric is `N`-dependent on the rust
side**, so it is only comparable across runs that used the same `N` (the `case_<name>_calls` companion
records it: sys 1 500 000 / 1 500 000 / 3 000 000 / 3 000 000; rust 20 000 / 15 000 / 25 000 / 15 000).

`list_walk` (BIF-free recursion — the per-element dispatch signal):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 7.1059 | **absent** (`marginal below timer resolution`) | — |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | (per-runtime `N`, not a ratio) |
| `case_list_walk_peak_rss_bytes` p50 | 66.42 MiB | 376.44 MiB | boot-inflated |

The rust `ns_per_element` is absent by the workload's own honest rule (the rust marginal did not resolve
at `--full`'s sample count); **no number is fabricated** for it.

`copy_bound` (term-copy boundary):

| case | sys ns/op | rust ns/op | rust/sys | ops sys / rust | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 812.0279 | **absent** (`marginal below timer resolution`) | — | 1 500 000 / 5 000 | 65.96 | 382.54 |
| `ets_copy` | 340.0868 | 15 470.9003 | **45.49×** | 2 000 000 / 100 000 | 66.89 | 450.12 |
| `send_recv` | 1 787.0913 | 217 031.8844 | **121.44×** | 500 000 / 2 000 | 66.15 | 382.63 |
| `send_recv_plain` | 740.7461 | **173 712.8155** (m12: failed — timed out) | **234.51×** | 1 000 000 / 2 000 | 66.34 | 382.27 |

`copy_bound` whole-workload `peak_memory_bytes`: sys 71 573 504 (68.26 MiB) → rust 472 092 672 (450.22
MiB). The rust row is `4 ok, 0 unsupported (of 4)` with `etf_roundtrip`'s marginal honestly absent.

`gc_bound`:

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 7 967.6115 | 330 493.5681 | 41.48× |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | (per-runtime `N`) |
| `case_churn_peak_rss_bytes` p50 | 66.03 MiB | 376.42 MiB | boot-inflated |

Deterministic counters (contention-immune): `case_*_ops` = 1 500 000 / 2 000 000 / 500 000 / 1 000 000
(sys `copy_bound`), 5 000 / 100 000 / 2 000 / 2 000 (rust `copy_bound`); `case_*_calls` = 1 500 000 /
1 500 000 / 3 000 000 / 3 000 000 (sys `foreign`), 20 000 / 15 000 / 25 000 / 15 000 (rust `foreign`);
`case_list_walk_elements` = 1e8 sys / 1e6 rust; `case_churn_ops` = 200 000 sys / 2 000 rust. All
observed stdout matched the `N × 1` / `N × 500500` constant on both sides.

## 6. Tier effect (Rust vs Rust — **not** a system-ERTS comparison)

`rust` default tier / `rust` interpreter tier, read from the **matched-form `--quick`** pair
(`m13-compare-tier-effect-quick.txt`: `a` = interpreter, `b` = default). `> 1` means the default
(baseline-JIT) tier is **dearer**; `< 1` means the interpreter is dearer. Both runs are `--quick` and
the load log bracketing them samples **59.30 → 87.12** (one window, 20:58:34 → 21:02:49; the reports
were written at 21:00:42 and 21:02:56), so their diffs are load-sensitive.

| workload | metric | interpreter | default (baseline JIT) | default/interp |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 798.2448 | 873.2962 | 1.094× (default dearer) |
| `baseline` | `compute_loop_ms` | 869.8937 | 1 148.4262 | 1.320× (default dearer) |
| `baseline` | `vm_memory_total_bytes` | 15 889 338 | 15 889 338 | 1.000× (parity) |
| `list_walk` | `ns_per_element` | **419.6419** | **absent** ("marginal below timer resolution") | — (only the interpreter measured it) |
| `gc_bound` | `churn` ns/op | 184 403.4138 | 219 562.2748 | 1.191× (default dearer) |
| `copy_bound` | `etf_roundtrip` ns/op | 115 978.6001 | 244 878.2264 | 2.111× (default dearer) |
| `copy_bound` | `ets_copy` ns/op | 15 568.0181 | 11 230.5040 | 0.721× (default cheaper) |
| `copy_bound` | `send_recv` ns/op | 127 216.7958 | 81 352.0790 | 0.639× (default cheaper) |
| `copy_bound` | `send_recv_plain` ns/op | 19 878.3013 | **absent** | — (only the interpreter measured it) |
| `foreign` | 4 cases ns/call | 35 966.92–114 443.12 | 10 429.11–107 310.13 | 0.29×–1.07× (mixed) |
| `slice` | 9 cases wall_ms | 666.30–1 036.74 | 627.43–1 072.99 | 0.87×–1.48× (boot-dominated; no stable tier effect) |

Facts to carry forward for the tier table:

- **No clean default-tier win at m13.** At m12 the default (baseline-JIT) tier was a clear win on
  `gc_bound churn` (2.07×) and marginally cheaper on the `foreign` cases, at parity elsewhere; at m13 —
  under a much heavier, differently-placed load window — the matched `--quick` pair shows the
  interpreter **cheaper** on `gc_bound churn` (default 1.19× dearer) and on `copy_bound`
  `etf_roundtrip` (default 2.11× dearer), and the default **cheaper** on `ets_copy` (0.72×) and
  `send_recv` (0.64×). None of these is a stable tier effect on this host, and the two batches' tier
  tables are not comparable across the load regimes.
- **Two rust marginals did not resolve in the default-tier `--quick` run** (`list_walk`
  `ns_per_element`, `copy_bound` `send_recv_plain` `ns_per_op`); the interpreter run measured both. So
  no default-vs-interpreter pair exists for either case at m13.
- The as-specified artifact `m13-compare-tier-effect.txt` (interpreter `--quick` vs the **default
  `--full`** `m13-bench-xrun.json`) is a **form mismatch**; it is archived for completeness, and the
  matched-form `m13-compare-tier-effect-quick.txt` is the one this table is read from.
- Every tier table here is a **Rust-vs-Rust observation, NOT a system-ERTS comparison** (both files have
  no `sys` rows).

## 7. Sys-only external targets (`m13-bench-external.json`, `--full`)

| workload | status | headline | sys peak RSS |
|---|---|---|---|
| `erlc_compiler` | `ok` | wall 9 679.61 ms; 6.199 files/s (60 produced over 46 503 LOC) | 130 109 440 B |
| `elixir_compiler` | `ok` | wall 73 993.35 ms; 3.014 files/s (105 attempted / 223 produced); compiler startup 531.15 ms | 208 650 240 B |
| `cowboy` | `ok` | 61 363.95 req/s; p50 0.1460 ms / p99 2.1605 ms (307 086 requests, 0 errors); build 59 013.92 ms | 160 616 448 B |
| `khepri` | `ok` | `rebar3 eunit` wall 89 131.58 ms | 387 690 496 B |
| `plb` (`m13-bench-plb.json`) | `ok` | wall 122 157.47 ms; `test_time_ms` mean 1 326.31 / p50 700.51 / p99 4 513.28 (n=10); resolved `dotnet` 9.0.318 | 169 918 464 B |
| `emqx` (`m13-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number | — |

**Every one of these rows is executed by its own system-ERTS toolchain** (`erlc`, `elixirc`, the Cowboy
release on the oracle `erl`, `rebar3`'s Erlang, the `.NET` tool for PLB). **None** is executed by
the Rust runtime — they are **reference** figures for the sys-only surfaces, not Rust-runtime results.
Per-target detail is in `m13-tool-targets-notes.md`.

The whole batch ran in the `21:03:00 → 21:09:30` window at 1-min load **69.46–99.87** (median 81.97) —
roughly 10× the load the m12 external pass saw, so **every** wall/throughput figure in §7 and every
m12→m13 external delta in §8 is contention-dominated, not a capability change.

## 8. m13 vs m12 (`m13-compare-vs-m12-xrun.txt` / `m13-compare-vs-m12-external.txt`)

> **Record-vs-record, cross-host-load.** `a` = the m12 record (`m12-bench-xrun.json`, taken in a
> **quiet** window, 1-min load `5.77 → 2.09`; `m12-bench-external.json` at `10.91 → 9.34`); `b` = the
> m13 record (1-min load median **113.13** for the xrun, **81.97** for the external pass). The two sides
> were measured under very different machine conditions, so **these deltas are not a like-for-like
> comparison** — they mix real HEAD-vs-m12 movement with load. The tool diffs the **mean**.

Rust-default-tier rows:

| (workload, metric) | m12 | m13 | Δ |
|---|---|---|---|
| `baseline` `startup_ms` **rust** | 2 011.9543 | **1 061.4786** | **−47.24 %** |
| `baseline` `compute_loop_ms` **rust** | 2 059.9596 | **1 325.9140** | −35.63 % |
| `baseline` `vm_memory_total_bytes` **rust** | 15 892 346 | 15 889 338 | −0.02 % |
| `copy_bound` `send_recv` ns/op **rust** | 25 447 011.2735 | **217 031.8844** | **−99.15 %** |
| `copy_bound` `ets_copy` ns/op **rust** | 2 876.4047 | **15 470.9003** | +437.86 % |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 40 882.4466 | **absent** (`metric-only-in-a`) | the m13 marginal did not resolve |
| `copy_bound` `send_recv_plain` ns/op **rust** | **failed (60 s deadline)** | **173 712.8155** | the case now completes (`metric-only-in-b`) |
| `foreign` `emqx_base62_encode` ns/call **rust** | 15 675.9526 | 74 352.5495 | +374.31 % |
| `foreign` `bstr_to_lower` ns/call **rust** | 26 890.9337 | 44 293.3693 | +64.71 % |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 23 283.5499 | 33 094.2801 | +42.14 % |
| `foreign` `emqx_base62_decode` ns/call **rust** | 16 851.8374 | 21 624.2808 | +28.32 % |
| `gc_bound` `churn` ns/op **rust** | 108 250.5757 | 330 493.5681 | +205.30 % |
| `list_walk` ns/element **rust** | **absent** | **absent** | neither pass resolved it |
| `slice` rust walls | 2 076.98–2 443.59 ms | 856.63–1 082.15 ms | −47.9 % to −59.4 % |

`sys`-side rows moved at least as much (`baseline` `startup_ms` **+189.01 %**, `list_walk`
ns/element **+112.54 %**, `gc_bound` churn **+79.09 %**, `copy_bound` ns/op **+6.42 % to +40.28 %**,
`foreign` ns/call **+13.10 % to +49.84 %**, `slice` walls **+94 % to +276 %**) — i.e. the m13 window was
far heavier than the m12 quiet re-take, and the `sys` movement is the **control** that shows most of the
m12→m13 rust movement is host load, not a regression. The **one structural rust move** that is not
amplified by load in the same direction is `copy_bound` `send_recv` (−99.15 %) and `baseline`
`startup_ms` (−47.24 %), both consistent with the A/B's own finding that the m12 `run`-route cost is
gone at HEAD (§9).

External targets (m12 → m13, all inside the load caveat above):

| workload | m12 | m13 | Δ |
|---|---|---|---|
| `erlc_compiler` `wall_ms` | 6 842.1774 | 9 679.6055 | +41.47 % |
| `erlc_compiler` `files_per_sec` | 8.7691 | 6.1986 | −29.31 % |
| `elixir_compiler` `wall_ms` | 50 304.3387 | 73 993.3456 | +47.09 % |
| `elixir_compiler` `files_per_sec` | 4.4330 | 3.0138 | −32.02 % |
| `elixir_compiler` `compiler_startup_ms` | 297.6431 | 531.1545 | +78.45 % |
| `cowboy` `requests_per_sec` | 165 489.2135 | 61 363.9504 | −62.92 % |
| `cowboy` `build_ms` | 42 527.1090 | 59 013.9151 | +38.77 % |
| `cowboy` `latency_ms` (mean) | 0.1057 | 0.2679 | +153.55 % |
| `khepri` `wall_ms` | 77 564.3355 | 89 131.5793 | +14.91 % |

(No tool diff is archived for `plb`; read it from `m12-bench-plb.json` → `m13-bench-plb.json` —
70 648.46 → 122 157.47 ms. All are shared-host numbers, and none is a like-for-like comparison.)

## 9. The matched-`N` A/B, the CLI probe, and the boot status

### 9.1 Matched-`N` three-way A/B — HEAD / m12 / m10 (`m13-ab-{head,m12,m10}-r{1,2,3}.json`)

**Design.** The same driver command is run three times per round —
`run --quick --runtime rust --workloads baseline,copy_bound,foreign` — with only **`$BEAM_RT`
overridden** to each variant's own `beam_rt`:

| variant | binary | `runtimes[].program` in the report |
|---|---|---|
| `head` | HEAD `a141d6878c` | `erts_rust/target/release/beam_rt` |
| `m12`  | m12 tree `ee3e426db0` | `$TMPDIR/m12wt/erts_rust/target/release/beam_rt` |
| `m10`  | m10 tree `b4593818b3` | `$TMPDIR/m10wt/erts_rust/target/release/beam_rt` |

So the **argv is identical on all three sides and the case-table `N` is identical on all three sides**
(the embedded `bench_copy_bound`/`bench_foreign_driver` programs come from the *same* committed embedded
source and are built by the *same* system `erlc`, so all three sides execute identical bytecode);
**only the runtime binary changes**. Three rounds were run, each round interleaved in the order
`head → m12 → m10` (artifact completion timestamps `20:20:28 → 20:26:44`, `20:27:55 → 20:34:10`,
`20:35:31 → 20:42:10`).

**Which rounds are used.** **Rounds 1–2 are the robust estimator** (the median of the two per-variant
rounds); **round 3 is excluded**: it ran into a **load spike ≈`124`** (a session observation — no
committed log covers the A/B window, which ended before the xrun load log began), and the artifacts
agree — its rows are visibly inflated relative to rounds 1–2 (e.g. `head` `copy_bound.send_recv` ns/op
**350 076.5** in r3 vs **68 928.5 / 28 143.5** in r1/r2; `m10` `copy_bound.send_recv` **235 853.2** in
r3 vs **136 450.4 / 119 740.4**). Every m13 A/B report is `--quick` (3 runs/case), so even rounds 1–2
are noisy.

Rounds 1–2, per-variant raw values `r1 / r2` with the median in parentheses:

| metric | `head` | `m12` | `m10` |
|---|---|---|---|
| `baseline.startup_ms` | 590.2 / 673.4 (**631.8**) | 2 041.2 / 2 125.0 (**2 083.1**) | 20.6 / 16.0 (**18.3**) |
| `copy_bound.etf_roundtrip_ns_per_op` | 42 329.6 / 47 852.5 (**45 091.0**) | 44 812.4 / 44 036.1 (**44 424.2**) | 11 070.8 / 10 807.0 (**10 938.9**) |
| `copy_bound.ets_copy_ns_per_op` | 8 924.7 / 9 456.5 (**9 190.6**) | 2 991.7 / 2 691.4 (**2 841.5**) | 4 057.2 / 3 997.2 (**4 027.2**) |
| `copy_bound.send_recv_ns_per_op` | 68 928.5 / 28 143.5 (**48 536.0**) | 25 425 030.5 / 25 477 673.6 (**25 451 352.0**) | 136 450.4 / 119 740.4 (**128 095.4**) |
| `copy_bound.send_recv_plain_ns_per_op` | absent (r1) / 132 270.8 | absent / absent | 115 360.2 / 107 935.3 (**111 647.7**) |
| `foreign.bstr_to_lower_ns_per_call` | 26 970.0 / 26 163.3 (**26 566.6**) | 30 101.9 / 24 971.0 (**27 536.4**) | 21 420.7 / 18 699.8 (**20 060.3**) |
| `foreign.bstr_capitalize_token_ns_per_call` | 29 671.9 / 24 804.5 (**27 238.2**) | 23 335.4 / 25 471.2 (**24 403.3**) | 39 538.0 / 25 205.5 (**32 371.8**) |
| `foreign.emqx_base62_encode_ns_per_call` | 69 056.8 / 78 438.9 (**73 747.9**) | 14 982.7 / 19 761.9 (**17 372.3**) | 21 489.4 / 20 410.9 (**20 950.2**) |
| `foreign.emqx_base62_decode_ns_per_call` | 15 415.1 / 11 695.4 (**13 555.2**) | 18 144.3 / 22 066.3 (**20 105.3**) | 22 116.4 / 21 416.4 (**21 766.4**) |

Read only the **directions** (the host was drifting throughout):

- **`baseline.startup_ms`**: m10 **18.3 ms** ≪ HEAD **631.8 ms** ≪ m12 **2 083.1 ms** — the ≈2.1 s fixed
  `run`-route boot m12 recorded is **largely gone at HEAD** (≈0.63 s left), and m10 remains the floor.
- **`copy_bound.send_recv`** (the blocked-round signal): m12 ≈**25.45 ms/op** ≫ m10 ≈**128 µs/op** and
  HEAD ≈**48.5 µs/op** — i.e. the m12 ~25.5 ms-per-blocked-round cost is **much reduced at HEAD** (HEAD
  is *below* m10 on this case in the matched rounds). This is the same defect `m12-run-route-latency.md`
  recorded as OPEN.
- **`copy_bound.send_recv_plain`** produced metrics at HEAD (r2) and at m10, but not at m12 (where m12's
  xrun had hit the 60 s deadline) — consistent with the blocked-round improvement.
- **`copy_bound.etf_roundtrip`**: HEAD ≈45.1 µs and m12 ≈44.4 µs (parity) are **≈4.1×** m10's ≈10.9 µs;
  `ets_copy`: HEAD ≈9.19 µs is ≈2.3× m10's ≈4.03 µs while m12 ≈2.84 µs is *below* m10.
- **`foreign`**: HEAD is ≈1.3× m10 on `bstr_to_lower` and ≈3.5× m10 on `emqx_base62_encode`, but *below*
  m10 on `emqx_base62_decode` (0.62×) and `bstr_capitalize_token` (0.84×) — a mixed picture, and this
  workload's per-call metric is `N`-dependent on the rust side at every variant, so it is a shape
  indicator only.

### 9.2 CLI latency probe (`m13-cli-probe.csv`)

**Design.** Interleaved **5 reps × 3 probes × {head, m12, m10, sys}** — 55 rows. The probes are
`-noshell -eval 'halt().'` (`startup`), `-version` (`version`) and `-boot bootstrap/bin/start` (`boot`),
the **`boot` probe being run with `cwd` = each variant's own worktree root** so the relative boot-script
path resolves. The `sys` variant has **no `boot` rows** (the system `erl` has no such in-tree script),
which is why `sys` contributes 2 probes × 5 reps = 10 rows. The window is the session's **only quiet
one**: 1-min load **20.11–23.63** over `21:15:18 → 21:15:53`.

Binary provenance (sha256 prefixes, as recorded in the CSV): `head` `2f7f0b3c04d6`, `m12`
`325848e5b4d9`, `m10` `49f48c054b15`; `sys` records `-` (no in-tree binary).

Medians of the five reps (ms; the five raw reps are given so the median is auditable):

| probe | `head` | `m12` | `m10` | `sys` |
|---|---|---|---|---|
| `startup` (`-noshell -eval 'halt().'`) | **721** (698, 717, 721, 725, 778) | **2 101** (2 094, 2 100, 2 101, 2 103, 2 118) | **20** (20, 20, 20, 23, 24) | **200** (176, 199, 200, 212, 227) |
| `-version` | **3** (2, 3, 3, 4, 8) | **4** (2, 2, 4, 4, 5) | **3** (2, 2, 3, 3, 3) | **21** (15, 21, 21, 22, 25) |
| `-boot bootstrap/bin/start` | **733** (708, 717, 733, 733, 785), **exit 0** | **2 113** (2 104, 2 109, 2 113, 2 117, 2 199), **exit 0** | **430** (395, 412, 430, 436, 440), **exit 0** | — (not applicable) |

Every probe exited `0` on every rep. Reading:

- Bare startup: HEAD **721 ms** vs m12 **2 101 ms** vs m10 **20 ms** vs sys **200 ms** — the ~2.1 s
  m12 `run`-route startup cost is **reduced ≈2.9×** at HEAD but still ≈36× m10 and ≈3.6× the *system*
  `erl`. **The `run`/`-eval`/`-boot` startup cost is still a large open gap.**
- `-version` is cheap on every runtime (2–8 ms) and ~21 ms on `sys`; it plainly short-circuits the
  scripted-boot path that the bare-startup probe pays (HEAD: 721 ms bare vs 3 ms `-version`).
- Scripted boot (`-boot bootstrap/bin/start`) tracks bare startup: HEAD 733 ms, m12 2 113 ms, m10 430 ms
  — all exit `0` (boot **completes** on every variant).

### 9.3 Boot status probe at HEAD

A factual probe, not a workload:

```
$ (cwd = HEAD worktree root) erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=0        wall ≈0.73 s        (no stdout, no stderr)
```

- **Boot completes** at HEAD, as at m12 — and the fixed cost has fallen from m12's **≈2.07–2.09 s** to
  **≈0.73 s** (the CLI probe's `boot` median is 733 ms, exit 0, over 5 reps). The m11 record's
  `exit 3` / "directive 18 of 22" remains obsolete.
- The output is **empty** on both streams at this commit, so the probe's observable is its exit status
  and its wall time, nothing else.

### 9.4 `foreign` N-ladder (`m13-foreign-ladder.csv`) — corroboration only

Interleaved **5 passes × {head, m12, m10, sys} × {enc, cap, dec, lower} × N = 0/5 000/10 000/25 000/
60 000**; the per-row 1-min loadavg is a column in the CSV (**min `15.25` / max `91.01`; first row
`89.53`, last row `23.29`). The reported figure is the **startup-cancelled marginal**
`(wall@N − wall@0)/N` with the **median taken over the 5 passes**.

> **This ladder is NOISE-AMPLIFIED and is CORROBORATION ONLY.** It ran in a drifting window on a
> heavily loaded host, the per-pass sample is a single run per point, and the `sys` side's marginals
> even go negative at small `N` (a sub-timer-resolution wall difference). **The robust estimator for a
> HEAD-vs-m12-vs-m10 comparison is the matched-`N` driver A/B in §9.1**, not this table. It is kept
> because it is the only artifact that spans a 12× `N` range for all four variants.

Headline ratios (median-over-passes marginals; `head/X` > 1 means HEAD is dearer):

| probe | `head`/`m10` @25k | `head`/`m10` @60k | `head`/`m12` @25k | `head`/`m12` @60k |
|---|---|---|---|---|
| `enc` | **3.830×** | **4.515×** | **3.618×** | **4.171×** |

and the bands over the `N` grid:

| probe | `head`/`m10` band | `head`/`m12` band | shape |
|---|---|---|---|
| `enc` | 2.23×–4.52× (rising with `N`) | 1.51×–4.17× | HEAD's encode marginal grows with `N` |
| `cap` | **0.74×–0.88×** | 0.81×–1.25× | HEAD ≤ m10 |
| `dec` | **0.67×–0.74×** | 0.73×–0.84× | HEAD < m10 |
| `lower` | **0.68×–0.98×** | 0.76×–0.96× | HEAD ≤ m10 |

So the ladder agrees with the A/B on **direction** for three of the four probes (HEAD is *cheaper* than
m10 on `cap`/`dec`/`lower`) and shows the `enc` case as the one where HEAD is distinctly dearer — but
the exact factors in this table are noise, and the `enc` growth with `N` is exactly the N-dependence
the `foreign` row's own caveat warns about.

### 9.5 Driver test-suite status

**Untouched by this session.** This was a documentation pass only — no benchmark, no `cargo build`, no
`cargo run` and no `cargo test` was executed while producing this record. The driver's suite status is
therefore the standing one: **257 tests — 249 lib + 4 integration + 1 oracle smoke + 3 doc** (the
authoritative figures live in `../CONTEXT.md` / `RESULTS.md` and must not be restated as if re-measured
here).

## 10. Honesty check

- Every (workload × runtime) pair in `m13-bench-xrun.json` is `ok` with real metrics (**12/12**). No
  rust row is a `null`-only row, an `unsupported` classification or a crash/exit-3.
- **Two rust marginals are honestly absent, not fabricated:** `copy_bound` `etf_roundtrip` `ns_per_op`
  and `list_walk` `ns_per_element` each carry the driver's own
  `marginal below timer resolution; no … metric` note in the m13 xrun, and the `compare` diff records
  them as `only-sys` / `metric-only-in-a`. Their deterministic companions (`_ops` = 5 000,
  `_elements` = 1 000 000) are still emitted, so the loop length behind every present marginal is in
  the report.
- The **only** non-`ok` row in the whole session is `emqx` → `skipped(emqx-requires-otp-28)` — a verified
  toolchain constraint with **no** number (`m13-bench-emqx.json` carries `"metrics": []` and
  `"peak_memory_bytes": null`). See `m13-tool-targets-notes.md` and `m12-emqx-enablement-attempt.md`.
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`);
  each run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N` (or `/(N × LEN)` for `list_walk`), so per-process startup cancels
  on both sides.
- The tier tables are labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS) and
  `rust` (`beam_rt` default tier) explicitly. The two tier runs are both `--quick` (matched form); the
  as-specified `m13-compare-tier-effect.txt` is accompanied by `m13-compare-tier-effect-quick.txt`.
- **The loop workloads' rust peak RSS (≈376–450 MiB) is boot-dominated and NOT like-for-like**, and the
  `foreign`/`copy_bound` per-op rows are additionally `N`-dependent on the rust side — neither is a clean
  memory context. `slice` (one call per process) remains the per-process memory row in principle, but at
  m13 its rust RSS (≈377 MiB) is also the boot's resident set.
- **No m13 number should be quoted without its load window.** The host never went quiet for the
  measuring batches (only for the 35-second CLI probe), and the m12-vs-m13 deltas in §8 are a
  record-vs-record comparison across two very different load regimes.
- All committed artifact paths are normalised (no worktree/`$TMPDIR` absolutes).
